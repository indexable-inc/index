//! rnix CST -> `ir::Module`. Scope resolution happens here: every identifier
//! compiles to a (depth, slot) local access, a builtin reference, a constant
//! (`true`/`false`/`null` are ordinary bindings in cppnix's builtin scope),
//! or, when a `with` is in scope, a runtime `ResolveWith`. An identifier
//! that resolves to none of those is a compile-time "undefined variable",
//! matching cppnix's bindVars behavior.
//!
//! `rec` attrsets, `let`, and `inherit` all desugar to one shape: a frame of
//! mutually-visible thunk slots. The VM never sees recursion as a special
//! case.

use crate::builtins;
use crate::ir::{AttrSite, CodeUnit, Const, Formal, Module, NO_POS, Op, Param, SrcOrigin};
use crate::refusal::{Refusal, RefusalToken};
use crate::value2::normalize_path;
use rnix::ast::{self, AstToken, Expr, HasEntry};
use rowan::ast::AstNode;
use std::collections::HashMap;

#[derive(Debug)]
pub enum CompileError {
    Unimplemented(crate::refusal::Refusal),
    UndefinedVariable(String),
    Parse(String),
    /// Something cppnix's parser rejects by throwing a plain `Error` rather
    /// than a `ParseError`: `~/x` under `pure-eval` (`parser.y:455`) is the
    /// one today. Separate from [`CompileError::Parse`] because the bridge
    /// prefixes that one with `rust-eval parse error:`, which would put the
    /// failure in a different class from cppnix's for a failure cppnix does
    /// not call a parse error either.
    Eval(String),
}

type Result<T> = std::result::Result<T, CompileError>;

/// The one identifier spelling cppnix's parser turns into a position rather
/// than a variable reference.
const CUR_POS: &str = "__curPos";

/// The ops of one code unit as they are emitted, each with the byte offset of
/// the construct that emitted it.
///
/// The position is a cursor on the emitter rather than an argument to `push`,
/// so the hundred-odd `ops.push(...)` sites in this file did not have to grow
/// a second argument each -- and, more usefully, cannot fall out of step with
/// one. [`Compiler::compile`] sets the cursor from the node it is about to
/// compile and restores it on the way out, so an op emitted after a
/// sub-expression is attributed to the enclosing construct and not to
/// whatever was compiled last.
#[derive(Default)]
struct Emit {
    ops: Vec<Op>,
    spans: Vec<u32>,
    attr_sites: Vec<AttrSite>,
    /// Byte offset of the construct currently being compiled.
    at: u32,
}

impl Emit {
    fn new() -> Self {
        Emit {
            at: NO_POS,
            ..Emit::default()
        }
    }

    fn push(&mut self, op: Op) {
        self.ops.push(op);
        self.spans.push(self.at);
    }

    /// Append ops built elsewhere, all attributed to the current cursor.
    ///
    /// Only `let` and `rec` fill ops go through this, and they are all
    /// `Op::Thunk`, which cannot fail: the position they carry is never
    /// printed. Keeping them in the table anyway is what lets `spans` be
    /// parallel to `ops` unconditionally, which is the invariant every reader
    /// relies on.
    fn extend(&mut self, ops: Vec<Op>) {
        for op in ops {
            self.push(op);
        }
    }

    fn len(&self) -> usize {
        self.ops.len()
    }

    fn op_mut(&mut self, at: usize) -> Option<&mut Op> {
        self.ops.get_mut(at)
    }

    /// Record where the attributes of the `MkAttrs` about to be pushed were
    /// written. Called immediately before pushing it, so the site's `ip` is
    /// the index that op will take.
    fn attr_site(&mut self, mut names: Vec<(u32, u32)>, mut dynamic_names: Vec<(u32, u32)>) {
        if names.is_empty() && dynamic_names.is_empty() {
            return;
        }
        let static_count = u32::try_from(names.len()).unwrap_or(u32::MAX);
        names.append(&mut dynamic_names);
        self.attr_sites.push(AttrSite {
            ip: u32::try_from(self.ops.len()).unwrap_or(NO_POS),
            names,
            static_count,
            by_name: Box::default(),
        });
    }

    fn into_unit(self, param: Option<Param>) -> CodeUnit {
        CodeUnit {
            ops: self.ops,
            param,
            spans: self.spans,
            attr_sites: self.attr_sites,
        }
    }
}

/// The byte offset a syntax node starts at, as [`Emit`]'s cursor wants it.
fn offset_of(node: &rowan::SyntaxNode<rnix::NixLanguage>) -> u32 {
    u32::try_from(usize::from(node.text_range().start())).unwrap_or(NO_POS)
}

/// The byte offset a token starts at.
fn token_offset(token: &rowan::SyntaxToken<rnix::NixLanguage>) -> u32 {
    u32::try_from(usize::from(token.text_range().start())).unwrap_or(NO_POS)
}

/// The byte offset of a binary operator's own token: the first non-trivia
/// token of the `BinOp` node that starts after the left operand ends.
///
/// Found by walking the node's tokens rather than by name because rnix has no
/// `operator_token()` accessor and the spelling differs per operator; the
/// position it yields is `parser.y`'s `state->at(@2)`.
fn operator_offset(op: &ast::BinOp, lhs: &Expr) -> Option<u32> {
    let after = lhs.syntax().text_range().end();
    op.syntax()
        .children_with_tokens()
        .filter_map(|c| c.into_token())
        .find(|t| !t.kind().is_trivia() && t.text_range().start() >= after)
        .map(|t| token_offset(&t))
}

/// The byte offset of an attribute name in a path or an `inherit` list, which
/// is what `builtins.unsafeGetAttrPos` answers for the attribute it names.
fn attr_offset(attr: &ast::Attr) -> u32 {
    offset_of(attr.syntax())
}

/// The attribute name a `rec` set gives special meaning to: `state.s.overrides`
/// in cppnix (`eval.cc:1437`).
const OVERRIDES: &str = "__overrides";

/// One static scope frame: binding names in slot order.
enum ScopeFrame {
    Bindings(Vec<String>),
    With,
}

pub struct Compiler<'src> {
    module: Module,
    /// Where each pooled constant and symbol already sits.
    ///
    /// Without these, [`Compiler::konst`] and [`Compiler::intern`] scan
    /// everything emitted so far on every call, which is quadratic in a
    /// module's distinct constants and was 16% of a NixOS toplevel
    /// evaluation: 3,327,228,909 `Const` comparisons for 800,884 calls,
    /// an average scan depth of 4,154 where one lookup would do
    /// (ENG-12860). A few enormous modules dominate that, since the cost
    /// is per module and superlinear in its size.
    ///
    /// Compile-time only. `Module`'s serialized shape is unchanged -- the
    /// pool is still a `Vec` addressed by index -- and these two methods
    /// are the only code that reads or writes the vectors they index, so
    /// index and pool cannot drift apart.
    konst_idx: HashMap<Const, u32>,
    sym_idx: HashMap<String, u32>,
    scopes: Vec<ScopeFrame>,
    /// Base directory for relative path literals.
    base_dir: String,
    /// The settings this compilation resolves bare globals under.
    ///
    /// Compilation is settings-dependent and it is easy to miss why:
    /// `is_global` consults `pure-eval` (an impure-only constant is not a
    /// global when it is on) and `cpp-builtin-names` (a gated name is a global
    /// only if cppnix registered it), so the same text compiles to different
    /// ops under different settings. Carried as a value rather than read from
    /// `crate::eval` for the reason `Vm::settings` is.
    settings: &'src crate::eval::Settings,
}

/// Where the text being compiled came from, which is the whole of what
/// `__curPos` answers.
///
/// cppnix keeps this on the `PosTable` and `mkPos` reads it back
/// (`eval.cc:1019`): a `SourcePath` origin produces
/// `{ column; file; line; }`, and **anything else produces `null`** -- so
/// `nix eval --expr '__curPos'` is `null`, not a position into an imaginary
/// file. Verified against the fork's own binary rather than read off the
/// source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin<'a> {
    /// Text with no file behind it: `--expr`, a REPL line, one of this
    /// crate's embedded wrappers. `__curPos` is `null`.
    String,
    /// A file, named by the absolute path cppnix would print. Not
    /// canonicalised here: cppnix reports the path it resolved, symlinks and
    /// all, and a `realpath` on this side would answer a different string for
    /// the same file.
    File(&'a str),
    /// A file read through a lazily mounted input. `mount_point` is the exact
    /// logical store path used as the key in cppnix's `storeFS`.
    MountedFile { path: &'a str, mount_point: &'a str },
}

impl<'a> Origin<'a> {
    pub(crate) fn root(self) -> crate::value2::Root {
        match self {
            Origin::String | Origin::File(_) => crate::value2::Root::Ambient,
            Origin::MountedFile { mount_point, .. } => crate::value2::Root::mounted(mount_point),
        }
    }

    pub(crate) fn source_path(self) -> Option<&'a str> {
        match self {
            Origin::String => None,
            Origin::File(path) | Origin::MountedFile { path, .. } => Some(path),
        }
    }
}

pub fn compile_source(
    src: &str,
    base_dir: &str,
    origin: Origin<'_>,
    settings: &crate::eval::Settings,
) -> Result<Module> {
    compile_source_with_hook(src, base_dir, origin, settings, || {})
}

fn compile_source_with_hook(
    src: &str,
    base_dir: &str,
    origin: Origin<'_>,
    settings: &crate::eval::Settings,
    during_compile: impl FnOnce(),
) -> Result<Module> {
    // The parse is inside the measured phase deliberately: cppnix's
    // comparable number is `parseExprFrom`, and a compile timer that started
    // after the parse would be measuring a different thing than the arm it
    // gets compared against.
    let (out, nanos) = crate::perf::timed(|| {
        during_compile();
        compile_source_inner(src, base_dir, origin, settings)
    });
    crate::perf::note_compile(src.len(), nanos);
    out
}

#[cfg(test)]
pub(crate) fn compile_source_with_test_hook(
    src: &str,
    base_dir: &str,
    origin: Origin<'_>,
    settings: &crate::eval::Settings,
    during_compile: impl FnOnce(),
) -> Result<Module> {
    compile_source_with_hook(src, base_dir, origin, settings, during_compile)
}

fn compile_source_inner(
    src: &str,
    base_dir: &str,
    origin: Origin<'_>,
    settings: &crate::eval::Settings,
) -> Result<Module> {
    let parse = rnix::Root::parse(src);
    if let Some(err) = parse.errors().first() {
        return Err(CompileError::Parse(err.to_string()));
    }
    let root = parse.tree();
    let expr = root
        .expr()
        .ok_or_else(|| CompileError::Parse("empty expression".into()))?;
    let mut c = Compiler {
        module: Module {
            root: origin.root(),
            ..Module::default()
        },
        konst_idx: HashMap::new(),
        sym_idx: HashMap::new(),
        scopes: Vec::new(),
        base_dir: base_dir.to_owned(),
        settings,
    };
    // Before anything is compiled, because `emit_cur_pos` reads it and so
    // does every position an error or `unsafeGetAttrPos` reports. One pass
    // over the file; the alternative is keeping the source itself on the
    // module, which is far larger and would only ever be used to count these
    // same newlines.
    c.module.origin = origin
        .source_path()
        .map_or(SrcOrigin::String, |path| SrcOrigin::File(path.to_owned()));
    c.module.line_starts = Module::line_starts_of(src);
    let mut ops = Emit::new();
    c.compile(&expr, &mut ops)?;
    ops.push(Op::Ret);
    let unit = ops.into_unit(None);

    let entry = c.push_unit(unit)?;
    c.module.entry = entry;
    // Symbols are complete only now, so the sites' text index is built here,
    // once per unit, and the compiler<->VM contract is checked on every
    // module rather than only on the test corpus.
    let Module { units, symbols, .. } = &mut c.module;
    for unit in units.iter_mut() {
        unit.link_attr_sites(symbols).map_err(|detail| {
            CompileError::Parse(format!("internal: attr site contract: {detail}"))
        })?;
    }
    Ok(c.module)
}

/// The expressions cppnix's `maybeThunk` never thunks: an integer, float or
/// URI literal, a string or path with no interpolation (`<x>` is an
/// application, see `compile_path`), the empty list. Parentheses are looked
/// through, as cppnix's parser has no node for them.
fn is_constant(expr: &Expr) -> bool {
    match expr {
        Expr::Paren(p) => p.expr().is_some_and(|inner| is_constant(&inner)),
        Expr::Literal(_) => true,
        Expr::Str(s) => !s
            .normalized_parts()
            .iter()
            .any(|part| matches!(part, ast::InterpolPart::Interpolation(_))),
        Expr::Path(p) => {
            !p.parts()
                .any(|part| matches!(part, ast::InterpolPart::Interpolation(_)))
                && !p.syntax().text().to_string().starts_with('<')
        }
        Expr::List(l) => l.items().next().is_none(),
        _ => false,
    }
}

impl<'src> Compiler<'src> {
    fn push_unit(&mut self, u: CodeUnit) -> Result<u32> {
        let index = checked_unit_index(self.module.units.len())?;
        checked_op_count(u.ops.len())?;
        self.module.units.push(u);
        Ok(index)
    }

    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.sym_idx.get(s) {
            return i;
        }
        let i = self.module.symbols.len() as u32;
        self.module.symbols.push(s.to_owned());
        self.sym_idx.insert(s.to_owned(), i);
        i
    }

    fn konst(&mut self, c: Const) -> u32 {
        crate::perf::note_konst();
        if let Some(&i) = self.konst_idx.get(&c) {
            return i;
        }
        let i = self.module.consts.len() as u32;
        self.konst_idx.insert(c.clone(), i);
        self.module.consts.push(c);
        i
    }

    /// Compile `expr`'s ops into `ops` (the value ends on the stack).
    ///
    /// The one place the emitter's position cursor moves for an ordinary
    /// expression. It is restored afterwards so that an op the ENCLOSING
    /// construct emits after this call -- the `Op::Add` after both operands,
    /// the `Op::MkList` after every element -- is attributed to that
    /// construct rather than to whichever sub-expression happened to be
    /// compiled last. A few callers below move it again for one op, where
    /// cppnix reports a finer position than the whole node.
    fn compile(&mut self, expr: &Expr, ops: &mut Emit) -> Result<()> {
        let outer = ops.at;
        ops.at = offset_of(expr.syntax());
        let r = self.compile_inner(expr, ops);
        ops.at = outer;
        r
    }

    fn compile_inner(&mut self, expr: &Expr, ops: &mut Emit) -> Result<()> {
        match expr {
            Expr::Literal(lit) => self.compile_literal(lit, ops),
            Expr::Str(s) => self.compile_str(s, ops),
            Expr::Path(p) => self.compile_path(p, ops),
            Expr::Ident(id) => self.compile_ident(id, ops),
            Expr::Paren(p) => {
                let inner = p
                    .expr()
                    .ok_or_else(|| CompileError::Parse("empty parens".into()))?;
                self.compile(&inner, ops)
            }
            Expr::UnaryOp(op) => self.compile_unary(op, ops),
            Expr::BinOp(op) => self.compile_binop(op, ops),
            Expr::IfElse(ie) => self.compile_if(ie, ops),
            Expr::List(l) => self.compile_list(l, ops),
            Expr::LetIn(li) => self.compile_let(li, ops),
            Expr::Lambda(lam) => self.compile_lambda(lam, ops),
            Expr::Apply(ap) => self.compile_apply(ap, ops),
            Expr::AttrSet(a) => self.compile_attrset(a, ops),
            Expr::Select(sel) => self.compile_select(sel, ops),
            Expr::HasAttr(ha) => self.compile_hasattr(ha, ops),
            Expr::With(w) => self.compile_with(w, ops),
            Expr::Assert(a) => self.compile_assert(a, ops),
            Expr::LegacyLet(ll) => self.compile_legacy_let(ll, ops),
            other => Err(CompileError::Unimplemented(Refusal::new(
                RefusalToken::UnsupportedSyntax,
                node_name(other).to_owned(),
            ))),
        }
    }

    /// Compile `expr` as a fresh thunk unit capturing the current scope.
    ///
    /// A constant never comes here: `value_or_thunk` pushes it as its value
    /// first, in every lazy position, as cppnix's `maybeThunk` does.
    fn compile_thunk(&mut self, expr: &Expr) -> Result<Op> {
        let mut ops = Emit::new();
        self.compile(expr, &mut ops)?;
        ops.push(Op::Ret);
        let unit = ops.into_unit(None);

        let unit = self.push_unit(unit)?;
        Ok(Op::Thunk { unit })
    }

    /// cppnix's `Expr::maybeThunk`: a bare variable in a lazy position is
    /// passed as the binding's own cell rather than a fresh thunk over it, so
    /// two references to one binding ARE one value. That is the documented
    /// value-identity optimization, and it is what makes `[ f ] == [ f ]`
    /// true while `f == f` is false -- equality short-circuits on cell
    /// identity, which a per-reference thunk would destroy.
    ///
    /// Only for positions whose op runs in the environment it was compiled
    /// against. let/rec fill ops run one frame out from their own scope (the
    /// frame does not exist until PushEnv), so those keep `compile_thunk`,
    /// whose captured env PushEnv repoints.
    fn compile_lazy(&mut self, expr: &Expr) -> Result<Op> {
        if let Expr::Paren(paren) = expr
            && let Some(inner) = paren.expr()
        {
            return self.compile_lazy(&inner);
        }
        if let Expr::Ident(id) = expr
            && let Some(tok) = id.ident_token()
            // `__curPos` is not a variable at all, so a binding of that name
            // must not capture it here either. Without this line
            // `let __curPos = 1; in [ __curPos ]` answered `[ 1 ]` while
            // cppnix answers `[ { column = …; } ]`, because a list element is
            // a lazy position and this probe reaches `GetLocal` before
            // `compile_ident` is ever called.
            && tok.text() != CUR_POS
        {
            let mut probe = Emit::new();
            if self.compile_var(tok.text(), &mut probe).is_ok() {
                match probe.ops.as_slice() {
                    [Op::GetLocal { depth, slot }] => {
                        return Ok(Op::GetLocalLazy {
                            depth: *depth,
                            slot: *slot,
                        });
                    }
                    // A global that already is a value -- `true`, `false`,
                    // `null`, `builtins`, a bare primop -- is pushed as that
                    // value: cppnix's `ExprVar::maybeThunk` hands back the
                    // base environment's cell rather than allocating a thunk,
                    // which is why `flake = false` is forceable under the
                    // trivial rule there. Not `derivation`, `__nixPath` or an
                    // unimplemented global: those ops run code when executed
                    // and stay deferred.
                    [op @ (Op::Const(_) | Op::BuiltinsSet | Op::Builtin { .. })] => return Ok(*op),
                    _ => {}
                }
            }
        }
        self.value_or_thunk(expr)
    }

    /// The rest of cppnix's `maybeThunk`: `ExprInt`, `ExprFloat`,
    /// `ExprString` and `ExprPath` return their value and `ExprList` returns
    /// `vEmptyList` for `[ ]`, so a constant is never a thunk there, which is
    /// why `nixConfig.x = [ "s" ]` holds a string `readFlake` reads without
    /// forcing. Here each of those compiles to exactly one op that reads no
    /// environment (`Const`, `MkList { n: 0 }`), pushed in place of a thunk.
    /// Valid in every lazy position, the let/rec fill ops included: those run
    /// one frame out from their own scope, and a constant consults no frame.
    ///
    /// The probe compiles only `is_constant` shapes, which push no units, so a
    /// miss costs a constant-pool lookup and nothing is emitted twice.
    fn value_or_thunk(&mut self, expr: &Expr) -> Result<Op> {
        if let Expr::Paren(paren) = expr
            && let Some(inner) = paren.expr()
        {
            return self.value_or_thunk(&inner);
        }
        // Resolve in the actual scope: a binding named `false` must retain
        // its cell, while the builtin Boolean needs no recursive frame.
        if let Expr::Ident(id) = expr
            && let Some(tok) = id.ident_token()
            && tok.text() != CUR_POS
        {
            let mut probe = Emit::new();
            self.compile_var(tok.text(), &mut probe)?;
            if let [op @ (Op::Const(_) | Op::BuiltinsSet | Op::Builtin { .. })] =
                probe.ops.as_slice()
            {
                return Ok(*op);
            }
        }
        if is_constant(expr) {
            let mut probe = Emit::new();
            self.compile(expr, &mut probe)?;
            if let [op @ (Op::Const(_) | Op::MkList { n: 0 })] = probe.ops.as_slice() {
                return Ok(*op);
            }
        }
        self.compile_thunk(expr)
    }

    fn compile_literal(&mut self, lit: &ast::Literal, ops: &mut Emit) -> Result<()> {
        let c = match lit.kind() {
            ast::LiteralKind::Integer(i) => Const::Int(
                i.value()
                    .map_err(|e| CompileError::Parse(format!("bad integer literal: {e}")))?,
            ),
            ast::LiteralKind::Float(f) => Const::Float(
                f.value()
                    .map_err(|e| CompileError::Parse(format!("bad float literal: {e}")))?,
            ),
            ast::LiteralKind::Uri(u) => {
                let text = u.syntax().text().to_string();
                self.lint_url_literal(&text)?;
                Const::Str(text)
            }
        };
        let idx = self.konst(c);
        ops.push(Op::Const(idx));
        Ok(())
    }

    fn compile_str(&mut self, s: &ast::Str, ops: &mut Emit) -> Result<()> {
        let parts = s.normalized_parts();
        let mut n: u16 = 0;
        for part in &parts {
            match part {
                ast::InterpolPart::Literal(text) => {
                    let idx = self.konst(Const::Str(text.clone()));
                    ops.push(Op::Const(idx));
                    n += 1;
                }
                ast::InterpolPart::Interpolation(ip) => {
                    let inner = ip
                        .expr()
                        .ok_or_else(|| CompileError::Parse("empty interpolation".into()))?;
                    self.compile(&inner, ops)?;
                    n += 1;
                }
            }
        }
        if n == 0 {
            let idx = self.konst(Const::Str(String::new()));
            ops.push(Op::Const(idx));
        } else if n > 1 || !matches!(parts.first(), Some(ast::InterpolPart::Literal(_))) {
            // Interpolation coerces to string even for a single part.
            ops.push(Op::ConcatStrings { n });
        }
        Ok(())
    }

    /// cppnix's parser lints, at the sites its parser fires them.
    ///
    /// Each helper is one `diagnose(...)` call in `parser.y`, and each fires
    /// while *compiling*, as cppnix's fire while parsing: a linted literal in
    /// a branch nobody takes still fails the file. Only `fatal` produces
    /// anything -- at `warn` cppnix prints a diagnostic this backend does
    /// not, which is tier-2 warning text and stays out of the value
    /// comparison (the line drawn, with numbers, where the bridge used to
    /// refuse every evaluation under a `fatal` lint; ENG-12569, ENG-12597).
    ///
    /// The messages are cppnix's byte for byte, ` (lint-...)` suffix
    /// included -- `diagnose()` appends that from the setting's name
    /// (`diagnose.hh:58`). They must stay identical: the lang-diff gate
    /// classifies these failures as `unknown`, where the comparison is byte
    /// equality of the terminal `error:` line.
    ///
    /// [`CompileError::Eval`], not `Parse`: cppnix throws a `ParseError`,
    /// but its message has no "syntax error" for the class table to see,
    /// while the bridge prefixes `CompileError::Parse` with `rust-eval parse
    /// error:` -- which would put the two arms in different classes for the
    /// same failure.
    fn lint_url_literal(&self, literal: &str) -> Result<()> {
        // `parser.y:372-380`. Only the unquoted `URI` token lints; a quoted
        // URL lexes as a string on both arms and never reaches here.
        if self.settings.lint_url_literals.is_fatal() {
            return Err(CompileError::Eval(format!(
                "URL literals are disallowed. Consider using a string literal \
                 \"{literal}\" instead (lint-url-literals)"
            )));
        }
        Ok(())
    }

    /// See [`Self::lint_url_literal`]. `parser.y:419-424`.
    fn lint_absolute_path(&self, literal: &str) -> Result<()> {
        if self.settings.lint_absolute_path_literals.is_fatal() {
            return Err(CompileError::Eval(format!(
                "absolute path literals are not portable. Consider replacing path \
                 literal '{literal}' by a string, relative path, or parameter \
                 (lint-absolute-path-literals)"
            )));
        }
        Ok(())
    }

    /// See [`Self::lint_url_literal`]. `parser.y:461-466`: the home form has
    /// its own wording but reads `lintAbsolutePathLiterals`.
    fn lint_home_path(&self, literal: &str) -> Result<()> {
        if self.settings.lint_absolute_path_literals.is_fatal() {
            return Err(CompileError::Eval(format!(
                "home path literals are not portable. Consider replacing path \
                 literal '{literal}' by a string, relative path, or parameter \
                 (lint-absolute-path-literals)"
            )));
        }
        Ok(())
    }

    /// See [`Self::lint_url_literal`]. `parser.y:436-444`, including its
    /// guard: a literal already starting with `.` (`./x`, `../x`) never
    /// trips, which is what `eval-okay-dotslash-path-fatal` and its sibling
    /// pin.
    fn lint_short_path(&self, literal: &str) -> Result<()> {
        if literal.starts_with('.') {
            return Ok(());
        }
        if self.settings.lint_short_path_literals.is_fatal() {
            return Err(CompileError::Eval(format!(
                "relative path literal '{literal}' should be prefixed with '.' for \
                 clarity: './{literal}' (lint-short-path-literals)"
            )));
        }
        Ok(())
    }

    /// `~/x` -> `<home>/x`, cppnix's `HPATH` rule (`parser.y:453-468`).
    ///
    /// Three things happen there and all three happen here:
    ///
    /// * Under `pure-eval` it throws `the path '%s' can not be resolved in
    ///   pure mode` -- a plain `Error`, not a `ParseError`, hence
    ///   [`CompileError::Eval`]. It fires whether or not the literal is ever
    ///   demanded, because cppnix raises it while parsing; a home path in a
    ///   branch nobody takes still fails the file. It fires BEFORE the lint,
    ///   as in cppnix (`parser.y:455` before `:461`), which is why
    ///   `eval-fail-home-path-fatal` carries `--impure`.
    /// * The lint: [`Self::lint_home_path`], cppnix's
    ///   `diagnose(state->settings.lintAbsolutePathLiterals, ...)`.
    /// * `getHome().string() + std::string($1.p + 1, $1.l - 1)` -- textual
    ///   concatenation of the home directory with everything after the `~`,
    ///   which is why `~/foo` is `<home>/foo` and a bare `~/` is `<home>/`.
    ///   cppnix does not canonicalize here (the `HPATH` rule has no
    ///   `absPath`, unlike the `PATH` rule above it), and neither does this;
    ///   the interpolated form leaves the trailing slash for
    ///   `ExprConcatStrings` to canonicalize at the end, exactly as the
    ///   absolute and relative forms do.
    ///
    /// The message on a missing home is `getHomeOf`'s
    /// (`src/libutil/unix/users.cc:27`), because a missing home is the
    /// situation cppnix is in when it raises that one.
    fn home_path(&self, literal: &str) -> Result<String> {
        if self.settings.pure_eval {
            return Err(CompileError::Eval(format!(
                "the path '{literal}' can not be resolved in pure mode"
            )));
        }
        self.lint_home_path(literal)?;
        let home =
            self.settings.home_dir.as_deref().ok_or_else(|| {
                CompileError::Eval("cannot determine user's home directory".into())
            })?;
        // Concatenation and nothing else, because that is all cppnix does:
        // the `HPATH` rule has no `absPath` and no canonPath, unlike the
        // `PATH` rule above it. So a home of `/` gives `//foo` on both arms,
        // and the interpolated form's trailing slash survives to be
        // canonicalized once at the end, where `ExprConcatStrings` does it.
        let rest = literal.get(1..).unwrap_or_default();
        Ok(format!("{home}{rest}"))
    }

    fn compile_path(&mut self, p: &ast::Path, ops: &mut Emit) -> Result<()> {
        let parts: Vec<ast::InterpolPart<_>> = p.parts().collect();
        if parts
            .iter()
            .any(|part| matches!(part, ast::InterpolPart::Interpolation(_)))
        {
            return self.compile_interpolated_path(&parts, ops);
        }
        let text = p.syntax().text().to_string();
        // cppnix's parser does not build a path node for `<x>` at all: it
        // desugars the token into `__findFile __nixPath "x"` (parser.y, and
        // the comment at primops.cc:2242). Doing the same here rather than
        // resolving it directly is what makes a locally rebound `__nixPath`
        // or `__findFile` change the lookup, which is observable --
        // `eval-okay-search-path.nix` rebinds `__nixPath` in a `let` and
        // expects the inner list to win. ENG-12443.
        if let Some(inner) = text.strip_prefix('<').and_then(|t| t.strip_suffix('>')) {
            let name = inner.to_owned();
            self.compile_var("__findFile", ops)?;
            self.compile_var("__nixPath", ops)?;
            ops.push(Op::Apply);
            let idx = self.konst(Const::Str(name));
            ops.push(Op::Const(idx));
            ops.push(Op::Apply);
            return Ok(());
        }
        let rooted = if text.starts_with('/') {
            self.lint_absolute_path(&text)?;
            crate::value2::PathValue::ambient(normalize_path(&text))
        } else if text.starts_with('~') {
            crate::value2::PathValue::ambient(self.home_path(&text)?)
        } else {
            self.lint_short_path(&text)?;
            crate::value2::PathValue::normalized(
                self.module.root.clone(),
                &format!("{}/{}", self.base_dir, text),
            )
        };
        let idx = self.konst(Const::Path(rooted));
        ops.push(Op::Const(idx));
        Ok(())
    }

    /// `./${v}/x.patch`, which cppnix parses as `ExprConcatStrings` with
    /// `forceString` off over `[ExprPath(prefix), ...parts]`
    /// (`parser.y`, `path_start string_parts_interpolated PATH_END`).
    ///
    /// # The prefix keeps a trailing slash that a path value never has
    ///
    /// `path_start` makes the literal absolute and then **puts the trailing
    /// slash back**: `absPath("./", basePath)` is `/dir`, and the rule
    /// appends `/` because the literal ended in one (`parser.y:430` for the
    /// absolute branch, `:447` for the relative one). The concatenation is
    /// textual, so without that slash `./${v}/x` would be `/dir3.13/x`. Only
    /// the final result is canonicalized, which is why `ExprConcatStrings`
    /// passes `canonicalizePath = !first` and skips the first part -- its own
    /// comment calls it "the first path, which would only be not canonized in
    /// the first place if it's coming from a ./${foo} type path"
    /// (`eval.cc:2316`). This is that path.
    ///
    /// # Relative to the defining file, not the process
    ///
    /// `absPath` resolves against `state->basePath`, the directory of the
    /// file being parsed, so the same text in two files names two different
    /// paths and neither depends on where `nix` was invoked. `self.base_dir`
    /// is that directory here, and the corpus pins it.
    fn compile_interpolated_path(
        &mut self,
        parts: &[ast::InterpolPart<ast::PathContent>],
        ops: &mut Emit,
    ) -> Result<()> {
        // The grammar cannot produce anything else first: a path token starts
        // with `{PATH_CHAR}*/` (`lexer.l:124`, `PATH_SEG`), so an
        // interpolation can never open one.
        let Some(ast::InterpolPart::Literal(head)) = parts.first() else {
            return Err(CompileError::Parse(
                "a path literal must begin with a path segment".into(),
            ));
        };
        let literal = head.syntax().text().to_string();
        // The lints fire here too: cppnix's `path_start` production is
        // shared between the plain and interpolated forms, so `/x/${v}`
        // lints exactly as `/x/y` does.
        let (root, mut prefix) = if literal.starts_with('~') {
            (crate::value2::Root::Ambient, self.home_path(&literal)?)
        } else if literal.starts_with('/') {
            self.lint_absolute_path(&literal)?;
            (crate::value2::Root::Ambient, normalize_path(&literal))
        } else {
            self.lint_short_path(&literal)?;
            {
                let rooted = crate::value2::PathValue::normalized(
                    self.module.root.clone(),
                    &format!("{}/{}", self.base_dir, literal),
                );
                (rooted.root, rooted.path.to_string())
            }
        };
        // cppnix's `if (literal.size() > 1 && literal.back() == '/')`, read
        // off the literal and not off the normalized result: `/` alone
        // normalizes to `/` and must not become `//`.
        if literal.len() > 1 && literal.ends_with('/') && !prefix.ends_with('/') {
            prefix.push('/');
        }
        let idx = self.konst(Const::Path(crate::value2::PathValue::new(root, prefix)));
        ops.push(Op::Const(idx));

        let mut n: u16 = 1;
        for part in parts.iter().skip(1) {
            match part {
                ast::InterpolPart::Literal(text) => {
                    let idx = self.konst(Const::Str(text.syntax().text().to_string()));
                    ops.push(Op::Const(idx));
                }
                ast::InterpolPart::Interpolation(interpol) => {
                    let inner = interpol
                        .expr()
                        .ok_or_else(|| CompileError::Parse("empty interpolation".into()))?;
                    self.compile(&inner, ops)?;
                }
            }
            n = n
                .checked_add(1)
                .ok_or_else(|| CompileError::Parse("path with too many segments".into()))?;
        }
        ops.push(Op::ConcatPath { n });
        Ok(())
    }

    fn compile_ident(&mut self, id: &ast::Ident, ops: &mut Emit) -> Result<()> {
        let name = id
            .ident_token()
            .ok_or_else(|| CompileError::Parse("identifier without token".into()))?
            .text()
            .to_string();
        // cppnix's `expr_simple : ID` rule turns this one spelling into an
        // `ExprPos` node before it ever becomes a variable (`parser.y:348`),
        // so it is syntax and not a name: no `let`, lambda formal or `with`
        // shadows it, and it is handled here rather than in `compile_var`
        // for exactly that reason. `inherit __curPos;` goes through a
        // different production and stays an undefined variable on both arms.
        if name == CUR_POS {
            let offset = usize::from(id.syntax().text_range().start());
            return self.emit_cur_pos(offset, ops);
        }
        self.compile_var(&name, ops)
    }

    /// The position of the `__curPos` token at `offset`, as a constant.
    ///
    /// It is a constant because the answer is known at compile time: the
    /// compiler has the file name and the token's byte offset, and cppnix's
    /// answer is a function of exactly those two. No runtime position
    /// tracking is involved, which is what makes this a cheaper problem than
    /// `builtins.unsafeGetAttrPos` -- that one needs the position of an
    /// arbitrary attribute of an arbitrary set, reached through a value, so
    /// it goes through [`crate::value2::AttrOrigin`] and the per-unit
    /// `attr_sites` table instead (ENG-12137).
    ///
    /// ENG-12713: until this existed, `nixos/modules/tasks/filesystems/zfs.nix`
    /// raised `undefined variable '__curPos'` at compile time, and that module
    /// is in the default NixOS module list, so every fleet host's toplevel
    /// died on it.
    fn emit_cur_pos(&mut self, offset: usize, ops: &mut Emit) -> Result<()> {
        // `mkPos` answers null for an origin that is not a `SourcePath`
        // (`eval.cc:1034`), which is what `nix eval --expr '__curPos'`
        // returns.
        let SrcOrigin::File(file) = &self.module.origin else {
            let idx = self.konst(Const::Null);
            ops.push(Op::Const(idx));
            return Ok(());
        };
        let file = file.clone();
        let offset = u32::try_from(offset).unwrap_or(NO_POS);
        let (line, column) = self.module.line_col(offset).unwrap_or((0, 0));
        let (line, column) = (i64::from(line), i64::from(column));
        // Built by `MkAttrs` like every other attrset literal: the values on
        // the stack, the names in the op's attr site (with no position of
        // their own, which is what `unsafeGetAttrPos` answers `null` for).
        let mut sites = Vec::new();
        for (name, value) in [
            ("column", Const::Int(column)),
            ("file", Const::Str(file)),
            ("line", Const::Int(line)),
        ] {
            let value_idx = self.konst(value);
            ops.push(Op::Const(value_idx));
            sites.push((self.intern(name), NO_POS));
        }
        self.record_attr_site(ops, sites, Vec::new());
        ops.push(Op::MkAttrs {
            statics: 3,
            dynamics: 0,
        });
        Ok(())
    }

    fn compile_var(&mut self, name: &str, ops: &mut Emit) -> Result<()> {
        // Static scopes, innermost first. A static binding wins over any
        // inner `with`, and `depth` counts every environment node between
        // here and it, `with` scopes included: `lookup_local` steps through
        // an `EnvNode::With` the same as a frame.
        let mut depth: u16 = 0;
        let mut crossed_with = false;
        for frame in self.scopes.iter().rev() {
            match frame {
                ScopeFrame::Bindings(names) => {
                    if let Some(slot) = names.iter().position(|n| n == name) {
                        ops.push(Op::GetLocal {
                            depth,
                            slot: slot as u16,
                        });
                        return Ok(());
                    }
                    depth += 1;
                }
                ScopeFrame::With => {
                    crossed_with = true;
                    depth += 1;
                }
            }
        }
        // Builtin scope.
        match name {
            "true" => {
                let idx = self.konst(Const::Bool(true));
                ops.push(Op::Const(idx));
                return Ok(());
            }
            "false" => {
                let idx = self.konst(Const::Bool(false));
                ops.push(Op::Const(idx));
                return Ok(());
            }
            "null" => {
                let idx = self.konst(Const::Null);
                ops.push(Op::Const(idx));
                return Ok(());
            }
            _ => {}
        }
        if name == "builtins" {
            ops.push(Op::BuiltinsSet);
            return Ok(());
        }
        // `derivation` is the one global that is neither a primop nor a
        // constant: cppnix evaluates a Nix source file into it at startup
        // (`evalFile(derivationInternal, *vDerivation)`), after the builtins
        // exist, because the file uses them. Same shape here, one op that
        // thunks that source, so it stays lazy and an expression that never
        // mentions a derivation never compiles the wrapper.
        //
        // Placed after the local scopes above, so a binding named
        // `derivation` still shadows it. This dedicated opcode retains the
        // lazy wrapper, while ordinary function globals use builtin indices.
        if name == "derivation" {
            ops.push(Op::DerivationGlobal);
            return Ok(());
        }
        // `__nixPath` is the other global that is neither a primop nor a
        // constant this crate can write down: cppnix builds the list from the
        // `-I` flags and `NIX_PATH` at startup (`primops.cc:5564`), so only
        // the embedder knows it. Placed here for the same reason
        // `derivation` is -- after the local scopes, so a binding named
        // `__nixPath` still shadows it, which the corpus depends on.
        if name == "__nixPath" {
            ops.push(Op::NixPathGlobal);
            return Ok(());
        }
        // Global spellings and feature availability come from the owned catalogue.
        // The catalogue contains only implemented functions; unknown names remain
        // undefined rather than compiling to a placeholder opcode.
        if builtins::is_global(self.settings, name)
            && let Some(idx) = builtins::global_index(name.strip_prefix("__").unwrap_or(name))
        {
            ops.push(Op::Builtin { idx });
            return Ok(());
        }
        if crossed_with {
            let sym = self.intern(name);
            ops.push(Op::ResolveWith { sym });
            return Ok(());
        }
        Err(CompileError::UndefinedVariable(name.to_owned()))
    }

    fn compile_unary(&mut self, op: &ast::UnaryOp, ops: &mut Emit) -> Result<()> {
        let operand = op
            .expr()
            .ok_or_else(|| CompileError::Parse("unary op without operand".into()))?;
        self.compile(&operand, ops)?;
        match op.operator() {
            Some(ast::UnaryOpKind::Negate) => ops.push(Op::Negate),
            Some(ast::UnaryOpKind::Invert) => ops.push(Op::Not),
            None => return Err(CompileError::Parse("unknown unary operator".into())),
        }
        Ok(())
    }

    fn compile_binop(&mut self, op: &ast::BinOp, ops: &mut Emit) -> Result<()> {
        use ast::BinOpKind;
        let kind = op
            .operator()
            .ok_or_else(|| CompileError::Parse("unknown binary operator".into()))?;
        let (lhs, rhs) = match (op.lhs(), op.rhs()) {
            (Some(l), Some(r)) => (l, r),
            _ => return Err(CompileError::Parse("binary op missing operand".into())),
        };
        // Short-circuiting forms compile to jumps; the rest are strict.
        match kind {
            BinOpKind::PipeRight | BinOpKind::PipeLeft => {
                if !self.settings.pipe_operators {
                    // cppnix's lexer raises this before the operator means
                    // anything (`lexer.l:163-166` via
                    // `requireExperimentalFeature`): the feature decides
                    // whether the program is legal at all, so the same text
                    // must fail here too, and with the same terminal line --
                    // the lang-diff gate classifies it `unknown`, where the
                    // comparison is byte equality. `Eval`, not `Parse`, for
                    // the reason on [`Self::lint_url_literal`].
                    return Err(CompileError::Eval(
                        "experimental Nix feature 'pipe-operators' is disabled; add \
                         '--extra-experimental-features pipe-operators' to enable it"
                            .into(),
                    ));
                }
                // cppnix desugars both to a call and nothing else
                // (`parser.y:287-295`): `a |> f` is `makeCall(f, a)` and
                // `f <| a` is `makeCall(f, a)` -- function on the pipe's
                // pointed end. Two compiles and an `Apply`, no opcode.
                // Associativity is the parser's business (`|>` left, `<|`
                // right), and the rnix fork nests the CST the same way, so
                // each node here has exactly one function and one argument.
                let (f, a) = match kind {
                    BinOpKind::PipeRight => (&rhs, &lhs),
                    _ => (&lhs, &rhs),
                };
                // Function strict, argument as a thunk, exactly as
                // [`Self::compile_apply`] lowers `f a`: an `ExprCall`'s
                // argument is only forced when the function forces it.
                self.compile(f, ops)?;
                let t = self.compile_lazy(a)?;
                ops.push(t);
                // The call cppnix builds carries the operator's position
                // (`state->at(@2)`), so a failure inside the application is
                // reported there.
                ops.at = operator_offset(op, &lhs).unwrap_or(ops.at);
                ops.push(Op::Apply);
                Ok(())
            }
            BinOpKind::And => {
                self.compile(&lhs, ops)?;
                let jump_at = ops.len();
                ops.push(Op::JumpIfFalse { target: 0 });
                self.compile(&rhs, ops)?;
                let end = ops.len();
                ops.push(Op::Jump { target: 1 });
                // false branch: push false
                let f = self.konst(Const::Bool(false));
                ops.push(Op::Const(f));
                self.patch_jump(ops, jump_at, end + 1)?;
                Ok(())
            }
            BinOpKind::Or => {
                self.compile(&lhs, ops)?;
                ops.push(Op::Not);
                let jump_at = ops.len();
                ops.push(Op::JumpIfFalse { target: 0 });
                self.compile(&rhs, ops)?;
                let end = ops.len();
                ops.push(Op::Jump { target: 1 });
                let t = self.konst(Const::Bool(true));
                ops.push(Op::Const(t));
                self.patch_jump(ops, jump_at, end + 1)?;
                Ok(())
            }
            BinOpKind::Implication => {
                self.compile(&lhs, ops)?;
                let jump_at = ops.len();
                ops.push(Op::JumpIfFalse { target: 0 });
                self.compile(&rhs, ops)?;
                let end = ops.len();
                ops.push(Op::Jump { target: 1 });
                let t = self.konst(Const::Bool(true));
                ops.push(Op::Const(t));
                self.patch_jump(ops, jump_at, end + 1)?;
                Ok(())
            }
            _ => {
                self.compile(&lhs, ops)?;
                self.compile(&rhs, ops)?;
                // Where cppnix reports a failure of this operator, which is
                // not one rule. `a + b` is an `ExprConcatStrings` whose parts
                // carry their own positions and whose errors are
                // `.atPos(i_pos)` -- the operand being folded in, so the
                // right-hand one here (`eval.cc`, "integer overflow in
                // adding"). Every other operator is an `ExprCall` or an
                // `ExprOp*` built at `state->at(@2)` in `parser.y`, which is
                // the operator token.
                ops.at = if matches!(kind, BinOpKind::Add) {
                    offset_of(rhs.syntax())
                } else {
                    operator_offset(op, &lhs).unwrap_or(ops.at)
                };
                ops.push(match kind {
                    BinOpKind::Add => Op::Add,
                    BinOpKind::Sub => Op::Sub,
                    BinOpKind::Mul => Op::Mul,
                    BinOpKind::Div => Op::Div,
                    BinOpKind::Equal => Op::Eq,
                    BinOpKind::NotEqual => Op::Neq,
                    BinOpKind::Less => Op::Lt,
                    BinOpKind::LessOrEq => Op::Leq,
                    BinOpKind::More => Op::Gt,
                    BinOpKind::MoreOrEq => Op::Geq,
                    BinOpKind::Concat => Op::ConcatLists,
                    BinOpKind::Update => Op::Update,
                    other => {
                        return Err(CompileError::Unimplemented(Refusal::new(
                            RefusalToken::UnsupportedOperator,
                            format!("operator {other:?}"),
                        )));
                    }
                });
                Ok(())
            }
        }
    }

    fn patch_jump(&self, ops: &mut Emit, at: usize, dest: usize) -> Result<()> {
        let delta = (dest - at - 1) as u32;
        match ops.op_mut(at) {
            Some(Op::JumpIfFalse { target }) | Some(Op::Jump { target }) => {
                *target = delta;
                Ok(())
            }
            _ => Err(CompileError::Parse("internal: bad jump patch".into())),
        }
    }

    fn compile_if(&mut self, ie: &ast::IfElse, ops: &mut Emit) -> Result<()> {
        let cond = ie
            .condition()
            .ok_or_else(|| CompileError::Parse("if without condition".into()))?;
        let then = ie
            .body()
            .ok_or_else(|| CompileError::Parse("if without then".into()))?;
        let els = ie
            .else_body()
            .ok_or_else(|| CompileError::Parse("if without else".into()))?;
        self.compile(&cond, ops)?;
        let jf_at = ops.len();
        ops.push(Op::JumpIfFalse { target: 0 });
        self.compile(&then, ops)?;
        let j_at = ops.len();
        ops.push(Op::Jump { target: 0 });
        let else_start = ops.len();
        self.compile(&els, ops)?;
        let end = ops.len();
        self.patch_jump(ops, jf_at, else_start)?;
        self.patch_jump(ops, j_at, end)?;
        Ok(())
    }

    fn compile_list(&mut self, l: &ast::List, ops: &mut Emit) -> Result<()> {
        let mut n: u16 = 0;
        for item in l.items() {
            let t = self.compile_lazy(&item)?;
            ops.push(t);
            n += 1;
        }
        ops.push(Op::MkList { n });
        Ok(())
    }

    /// A thunk unit that builds one assembled set. Runs with the environment
    /// it was compiled in (a thunk captures the chain unchanged), so depths
    /// inside it need no adjustment.
    fn set_build_unit(&mut self, b: &SetBuild) -> Result<u32> {
        let mut ops = Emit::new();
        ops.at = b.pos;
        self.emit_set_build(b, &mut ops)?;
        ops.push(Op::Ret);
        let unit = ops.into_unit(None);
        self.push_unit(unit)
    }

    /// Emit the ops that leave one assembled set on the stack.
    fn emit_set_build(&mut self, b: &SetBuild, ops: &mut Emit) -> Result<()> {
        if b.rec {
            return self.emit_rec_set_build(b, ops);
        }
        // Static values first, then the dynamic (name, value) pairs: the
        // static names are not on the stack at all, they are the attr site's
        // static half, in this same emission order (every `inherit` first,
        // then the bindings, each in source order).
        let mut sites: Vec<(u32, u32)> = Vec::new();
        let mut dynamic_sites: Vec<(u32, u32)> = Vec::new();
        for inh in &b.inherits {
            self.emit_inherit_group(inh, ops, &mut sites)?;
        }
        for (name, pos, t) in &b.kids {
            let op = self.bind_value_op(t)?;
            ops.push(op);
            sites.push((self.intern(name), *pos));
        }
        for (attr, pos, t) in &b.dynamic {
            dynamic_sites.push((attr_count(dynamic_sites.len())?.into(), *pos));
            self.compile_attr_dynamic(attr, ops)?;
            let op = self.bind_value_op(t)?;
            ops.push(op);
        }
        // Counts come from the tables the op is zipped with, never from a
        // running counter: a counter that wrapped at 65_536 emitted
        // `MkAttrs { 0, 0 }` over a full site and a full stack.
        let statics = attr_count(sites.len())?;
        let dynamics = attr_count(dynamic_sites.len())?;
        self.record_attr_site(ops, sites, dynamic_sites);
        ops.push(Op::MkAttrs { statics, dynamics });
        Ok(())
    }

    /// The rec shape: a frame of mutually-visible slots, then a set built out
    /// of it. Dynamic names are evaluated inside the frame but do not join
    /// the scope, matching cppnix's dynamicEnv.
    ///
    /// # `__overrides`
    ///
    /// `rec { __overrides = o; ... }` is cppnix's one way to override an
    /// attribute that the rec's *other* attributes then see -- `//` cannot do
    /// it, because the siblings already closed over the original value
    /// (`eval.cc:1455`, the comment beginning "If the rec contains an
    /// attribute called `__overrides'"). `ExprAttrs::eval` implements it in
    /// two halves, and this emits both:
    ///
    /// * `eval.cc:1470` replaces `env2.values[j->second.displ]` for every
    ///   override name the rec also defines statically, which is what makes
    ///   a sibling see the override. Here that is one guard per slot:
    ///   `o.<name> or <original>`, with `o` read from a slot of its own.
    /// * `eval.cc:1471` `bindings.push_back(i)` for every override name the
    ///   rec does not define. Replace-plus-append over an attribute set is
    ///   exactly `//`, so the built set is `<statics> // o` -- one `Update`,
    ///   which also does the `state.forceAttrs` on `o` that cppnix does at
    ///   `eval.cc:1465`, at the same moment: while the set is built, not when
    ///   some attribute of it is demanded. `eval-fail-set-override` pins that
    ///   timing.
    ///
    /// The original `o` lives in a slot past the end of `names`, unreachable
    /// by name, because cppnix reads `vOverrides` out of the binding it made
    /// *before* the replacement loop can overwrite it -- an override set that
    /// itself carries `__overrides` replaces the attribute without changing
    /// what was consulted.
    ///
    /// Divergences from cppnix, both in wording only: `Update` says "expected
    /// a set but found an integer" where cppnix appends the value and a
    /// `while evaluating the `__overrides` attribute` trace note, and this
    /// backend carries no trace notes at all (ENG-12137).
    fn emit_rec_set_build(&mut self, b: &SetBuild, ops: &mut Emit) -> Result<()> {
        let mut names: Vec<String> = Vec::new();
        let mut where_written: Vec<u32> = Vec::new();
        for inh in &b.inherits {
            for attr in inh.attrs() {
                names.push(static_attr_name(&attr)?.ok_or_else(no_dynamic_in_inherit)?);
                where_written.push(attr_offset(&attr));
            }
        }
        names.extend(b.kids.iter().map(|(n, _, _)| n.clone()));
        where_written.extend(b.kids.iter().map(|(_, p, _)| *p));
        let overrides_at = names.iter().position(|n| n == OVERRIDES);
        self.scopes.push(ScopeFrame::Bindings(names.clone()));
        let result = (|| -> Result<()> {
            let mut fill = Vec::new();
            for inh in &b.inherits {
                self.rec_inherit_fill_ops(inh, &mut fill)?;
            }
            for (_, _, t) in &b.kids {
                // Thunks or constants, never GetLocalLazy: a fill op runs
                // before PushEnv, one frame out from the scope it was
                // compiled against, and only a constant reads no frame.
                fill.push(match t {
                    BindTree::Leaf(e) => self.value_or_thunk(e)?,
                    BindTree::Node(sub) => {
                        let unit = self.set_build_unit(sub)?;
                        Op::Thunk { unit }
                    }
                });
            }
            if let Some(ov) = overrides_at {
                // The extra slot holds the ORIGINAL `__overrides`; every
                // named slot, that one included, becomes a guard over it.
                let hidden = u16::try_from(fill.len())
                    .map_err(|_| CompileError::Parse("too many rec bindings".into()))?;
                let original = fill
                    .get(ov)
                    .copied()
                    .ok_or_else(|| CompileError::Parse("internal: no __overrides slot".into()))?;
                fill.push(original);
                for (slot, name) in names.iter().enumerate() {
                    // For `__overrides` itself the fallback reads the hidden
                    // slot rather than the expression again: two thunks over
                    // one binding would evaluate it twice, and cppnix has one
                    // Value cell.
                    let fallback = if slot == ov {
                        Op::GetLocalLazy {
                            depth: 0,
                            slot: hidden,
                        }
                    } else {
                        *fill
                            .get(slot)
                            .ok_or_else(|| CompileError::Parse("internal: slot gone".into()))?
                    };
                    let guard = self.override_guard(hidden, name, fallback)?;
                    *fill
                        .get_mut(slot)
                        .ok_or_else(|| CompileError::Parse("internal: slot gone".into()))? = guard;
                }
            }
            let n = u16::try_from(fill.len())
                .map_err(|_| CompileError::Parse("too many rec bindings".into()))?;
            ops.extend(fill);
            ops.push(Op::PushEnv { n });
            let mut sites: Vec<(u32, u32)> = Vec::new();
            for (slot, name) in names.iter().enumerate() {
                ops.push(Op::GetLocalLazy {
                    depth: 0,
                    slot: u16::try_from(slot)
                        .map_err(|_| CompileError::Parse("too many rec bindings".into()))?,
                });
                let sym = self.intern(name);
                sites.push((sym, where_written.get(slot).copied().unwrap_or(NO_POS)));
            }
            // With overrides the set is closed here and the appended names
            // arrive through `Update`; the dynamic attributes then land on
            // the result, which is cppnix's order and cppnix's duplicate
            // check. Without overrides the whole thing is one `MkAttrs`, as
            // it always was.
            if overrides_at.is_some() {
                let statics = attr_count(sites.len())?;
                self.record_attr_site(ops, core::mem::take(&mut sites), Vec::new());
                ops.push(Op::MkAttrs {
                    statics,
                    dynamics: 0,
                });
                ops.push(Op::GetLocalLazy {
                    depth: 0,
                    slot: names
                        .len()
                        .try_into()
                        .map_err(|_| CompileError::Parse("too many rec bindings".into()))?,
                });
                ops.push(Op::Update);
            }
            let mut dynamic_sites: Vec<(u32, u32)> = Vec::new();
            for (attr, pos, t) in &b.dynamic {
                dynamic_sites.push((attr_count(dynamic_sites.len())?.into(), *pos));
                self.compile_attr_dynamic(attr, ops)?;
                let op = self.bind_value_op(t)?;
                ops.push(op);
            }
            let dynamics = attr_count(dynamic_sites.len())?;
            if overrides_at.is_some() {
                if dynamics > 0 {
                    self.record_attr_site(ops, Vec::new(), dynamic_sites);
                    ops.push(Op::MkAttrsOnto { n: dynamics });
                }
            } else {
                let statics = attr_count(sites.len())?;
                self.record_attr_site(ops, sites, dynamic_sites);
                ops.push(Op::MkAttrs { statics, dynamics });
            }
            ops.push(Op::PopEnv);
            Ok(())
        })();
        self.scopes.pop();
        result
    }

    /// File the attribute names and positions of the `MkAttrs` about to be
    /// emitted.
    ///
    /// Static entries are `(module symbol, position)` in the order the op
    /// pops the static values (emission order: every `inherit` first, then
    /// the bindings), because the site IS where the VM takes those names
    /// from; the order is load-bearing, never sorted. Dynamic entries are
    /// `(index among the op's dynamic pairs, position)`, joined to the names
    /// the VM resolves on each execution. `CodeUnit::check_attr_sites` is the
    /// contract this has to meet.
    fn record_attr_site(
        &mut self,
        ops: &mut Emit,
        static_names: Vec<(u32, u32)>,
        dynamic_names: Vec<(u32, u32)>,
    ) {
        ops.attr_site(static_names, dynamic_names);
    }

    /// `<overrides>.<name> or <fallback>`, as one thunk for a rec slot.
    ///
    /// The soft select plus `OrDefault` pair is `?`-and-`or` semantics, which
    /// is what cppnix's `attrs->find(i.name)` amounts to from the slot's side:
    /// present in the override set means the override, absent means the
    /// binding the rec wrote. An override set that is not a set at all makes
    /// every guard miss -- cppnix's `1 ? a` is `false` too -- and the error is
    /// then raised where cppnix raises it, by the `forceAttrs` that `Update`
    /// stands in for.
    fn override_guard(&mut self, hidden: u16, name: &str, fallback: Op) -> Result<Op> {
        let sym = self.intern(name);
        // Synthesised, so there is no token to point at: this unit has no
        // counterpart in the source at all. `NO_POS` throughout rather than
        // the enclosing `rec`'s offset, because the one op here that can
        // fail (`SelectSoft` on a non-set cannot; `OrDefault` cannot) does
        // not exist, and a borrowed offset would name a line that did not
        // raise the error.
        let ops = vec![
            Op::GetLocal {
                depth: 0,
                slot: hidden,
            },
            Op::SelectSoft { sym },
            fallback,
            Op::OrDefault,
            Op::Ret,
        ];
        let spans = vec![NO_POS; ops.len()];
        let unit = self.push_unit(CodeUnit {
            ops,
            param: None,
            spans,
            attr_sites: Vec::new(),
        })?;
        Ok(Op::Thunk { unit })
    }

    /// One binding's value, in a position whose env matches its compilation.
    fn bind_value_op(&mut self, t: &BindTree) -> Result<Op> {
        match t {
            BindTree::Leaf(e) => self.compile_lazy(e),
            BindTree::Node(sub) => {
                let unit = self.set_build_unit(sub)?;
                Ok(Op::Thunk { unit })
            }
        }
    }

    /// `inherit x y;` / `inherit (e) x y;` as (name, value) pairs pushed onto
    /// `ops`; returns how many pairs. Values stay lazy.
    ///
    /// `sites` collects where each name was written. cppnix gives an
    /// inherited attribute the position of the NAME in the `inherit` list and
    /// not of whatever it was inherited from, which
    /// `eval-okay-inherit-attr-pos` pins for both spellings.
    fn emit_inherit_group(
        &mut self,
        inh: &ast::Inherit,
        ops: &mut Emit,
        sites: &mut Vec<(u32, u32)>,
    ) -> Result<()> {
        let mut fill = Vec::new();
        self.inherit_fill_ops(inh, &mut fill)?;
        for (attr, op) in inh.attrs().zip(fill) {
            let name = static_attr_name(&attr)?.ok_or_else(no_dynamic_in_inherit)?;
            ops.push(op);
            sites.push((self.intern(&name), attr_offset(&attr)));
        }
        Ok(())
    }

    /// `inherit x;` inside a rec scope resolves x in the ENCLOSING scope, so
    /// `rec { inherit x; }` takes the outer x rather than recursing on its
    /// own. Compiled with the new frame popped, then depths bumped by one:
    /// the thunk runs after PushEnv has repointed it into that frame.
    fn rec_inherit_fill_ops(&mut self, inh: &ast::Inherit, fill: &mut Vec<Op>) -> Result<()> {
        if inh.from().is_some() {
            // `inherit (e) x;`'s subject is an ordinary expression evaluated
            // in the rec scope, so it needs no adjustment.
            return self.inherit_fill_ops(inh, fill);
        }
        let frame = self
            .scopes
            .pop()
            .ok_or_else(|| CompileError::Parse("internal: scope underflow".into()))?;
        let mut outer = Vec::new();
        let r = self.inherit_fill_ops(inh, &mut outer);
        self.scopes.push(frame);
        r?;
        for op in outer {
            let Op::Thunk { unit } = op else {
                fill.push(op);
                continue;
            };
            if let Some(u) = self.module.units.get_mut(unit as usize) {
                for o in &mut u.ops {
                    if let Op::GetLocal { depth, slot } = *o {
                        *o = Op::GetLocal {
                            depth: depth + 1,
                            slot,
                        };
                    }
                }
            }
            fill.push(Op::Thunk { unit });
        }
        Ok(())
    }

    /// The lazy value op for each name in one inherit group.
    fn inherit_fill_ops(&mut self, inh: &ast::Inherit, fill: &mut Vec<Op>) -> Result<()> {
        let from = inh.from();
        for attr in inh.attrs() {
            let name = static_attr_name(&attr)?.ok_or_else(no_dynamic_in_inherit)?;
            let sym = self.intern(&name);
            let mut tops = Emit::new();
            tops.at = attr_offset(&attr);
            match &from {
                Some(f) => {
                    let fe = f
                        .expr()
                        .ok_or_else(|| CompileError::Parse("inherit (…) missing".into()))?;
                    self.compile(&fe, &mut tops)?;
                    tops.push(Op::Select { sym });
                }
                None => self.compile_var(&name, &mut tops)?,
            }
            tops.push(Op::Ret);
            let unit = self.push_unit(tops.into_unit(None))?;
            fill.push(Op::Thunk { unit });
        }
        Ok(())
    }

    fn compile_let(&mut self, li: &ast::LetIn, ops: &mut Emit) -> Result<()> {
        let b = build_entries(li, true, offset_of(li.syntax()))?;
        let mut names: Vec<String> = Vec::new();
        for inh in &b.inherits {
            for attr in inh.attrs() {
                names.push(static_attr_name(&attr)?.ok_or_else(no_dynamic_in_inherit)?);
            }
        }
        names.extend(b.kids.iter().map(|(n, _, _)| n.clone()));
        if !b.dynamic.is_empty() {
            // `let ${e} = v; in ...` is a parse error in cppnix: a binding
            // whose name is unknown until run time cannot be in scope.
            return Err(CompileError::Parse(
                "dynamic attributes not allowed in let".into(),
            ));
        }
        self.scopes.push(ScopeFrame::Bindings(names));
        let result = (|| -> Result<()> {
            let mut fill = Vec::new();
            for inh in &b.inherits {
                self.rec_inherit_fill_ops(inh, &mut fill)?;
            }
            for (_, _, t) in &b.kids {
                // As in `emit_rec_set_build`: thunks or constants, never
                // GetLocalLazy, since the fill runs before PushEnv.
                fill.push(match t {
                    BindTree::Leaf(e) => self.value_or_thunk(e)?,
                    BindTree::Node(sub) => {
                        let unit = self.set_build_unit(sub)?;
                        Op::Thunk { unit }
                    }
                });
            }
            let n = fill.len() as u16;
            ops.extend(fill);
            ops.push(Op::PushEnv { n });
            let body = li
                .body()
                .ok_or_else(|| CompileError::Parse("let without body".into()))?;
            self.compile(&body, ops)?;
            ops.push(Op::PopEnv);
            Ok(())
        })();
        self.scopes.pop();
        result
    }

    fn compile_lambda(&mut self, lam: &ast::Lambda, ops: &mut Emit) -> Result<()> {
        let param = lam
            .param()
            .ok_or_else(|| CompileError::Parse("lambda without parameter".into()))?;
        let body = lam
            .body()
            .ok_or_else(|| CompileError::Parse("lambda without body".into()))?;
        let (param_ir, names) = match &param {
            ast::Param::IdentParam(ip) => {
                let name = ip
                    .ident()
                    .and_then(|i| i.ident_token())
                    .ok_or_else(|| CompileError::Parse("lambda param without name".into()))?
                    .text()
                    .to_string();
                let sym = self.intern(&name);
                (Param::Ident(sym), vec![name])
            }
            ast::Param::Pattern(pat) => {
                let mut fields = Vec::new();
                let mut names = Vec::new();
                // Two passes: defaults see all fields (and the @-binding).
                for entry in pat.pat_entries() {
                    let name = entry
                        .ident()
                        .and_then(|i| i.ident_token())
                        .ok_or_else(|| CompileError::Parse("pattern field without name".into()))?
                        .text()
                        .to_string();
                    names.push(name);
                }
                let bind = match pat.pat_bind() {
                    Some(b) => {
                        let name = b
                            .ident()
                            .and_then(|i| i.ident_token())
                            .ok_or_else(|| CompileError::Parse("@ without name".into()))?
                            .text()
                            .to_string();
                        names.push(name.clone());
                        Some(self.intern(&name))
                    }
                    None => None,
                };
                self.scopes.push(ScopeFrame::Bindings(names.clone()));
                let defaults: Result<Vec<Formal>> = pat
                    .pat_entries()
                    .map(|entry| {
                        let token = entry.ident().and_then(|i| i.ident_token());
                        let name = token
                            .as_ref()
                            .map(|t| t.text().to_string())
                            .unwrap_or_default();
                        // The NAME token, not the whole `b ? d` entry:
                        // cppnix records `formal.pos` off the identifier
                        // (`parser.y`'s `formal: ID`), which is what
                        // `eval-okay-getattrpos-functionargs` expects.
                        let pos = token.as_ref().map_or(NO_POS, token_offset);
                        let sym = self.intern(&name);
                        let default = match entry.default() {
                            Some(d) => {
                                let mut dops = Emit::new();
                                self.compile(&d, &mut dops)?;
                                dops.push(Op::Ret);
                                Some(self.push_unit(dops.into_unit(None))?)
                            }
                            None => None,
                        };
                        Ok(Formal { sym, default, pos })
                    })
                    .collect();
                self.scopes.pop();
                fields.extend(defaults?);
                (
                    Param::Formals {
                        fields,
                        ellipsis: pat.ellipsis_token().is_some(),
                        bind,
                    },
                    {
                        let mut names = Vec::new();
                        for entry in pat.pat_entries() {
                            if let Some(t) = entry.ident().and_then(|i| i.ident_token()) {
                                names.push(t.text().to_string());
                            }
                        }
                        if let Some(b) = pat.pat_bind()
                            && let Some(t) = b.ident().and_then(|i| i.ident_token())
                        {
                            names.push(t.text().to_string());
                        }
                        names
                    },
                )
            }
        };
        self.scopes.push(ScopeFrame::Bindings(names));
        let result = (|| -> Result<u32> {
            let mut bops = Emit::new();
            self.compile(&body, &mut bops)?;
            bops.push(Op::Ret);
            self.push_unit(bops.into_unit(Some(param_ir)))
        })();
        self.scopes.pop();
        let unit = result?;
        ops.push(Op::Closure { unit });
        Ok(())
    }

    fn compile_apply(&mut self, ap: &ast::Apply, ops: &mut Emit) -> Result<()> {
        let f = ap
            .lambda()
            .ok_or_else(|| CompileError::Parse("apply without function".into()))?;
        let arg = ap
            .argument()
            .ok_or_else(|| CompileError::Parse("apply without argument".into()))?;
        self.compile(&f, ops)?;
        let t = self.compile_lazy(&arg)?;
        ops.push(t);
        ops.push(Op::Apply);
        Ok(())
    }

    /// `let { body = e; ... }`: cppnix's pre-`let ... in` syntax, defined as
    /// the `body` attribute of the equivalent rec set. Deprecated upstream and
    /// still all over the corpus.
    fn compile_legacy_let(&mut self, ll: &ast::LegacyLet, ops: &mut Emit) -> Result<()> {
        let b = build_entries(ll, true, offset_of(ll.syntax()))?;
        self.emit_set_build(&b, ops)?;
        let sym = self.intern("body");
        ops.push(Op::Select { sym });
        Ok(())
    }

    fn compile_attrset(&mut self, a: &ast::AttrSet, ops: &mut Emit) -> Result<()> {
        let b = absorb(a)?;
        self.emit_set_build(&b, ops)
    }

    fn compile_attr_dynamic(&mut self, attr: &ast::Attr, ops: &mut Emit) -> Result<()> {
        match attr {
            ast::Attr::Dynamic(d) => {
                let e = d
                    .expr()
                    .ok_or_else(|| CompileError::Parse("empty dynamic attr".into()))?;
                self.compile(&e, ops)
            }
            ast::Attr::Str(s) => self.compile_str(s, ops),
            ast::Attr::Ident(_) => Err(CompileError::Parse("internal: static attr".into())),
        }
    }

    /// Whether a bare `builtins` here is the global set rather than something
    /// a `let` or a lambda parameter bound.
    ///
    /// The same test `compile_var` makes, and it has to stay the same test:
    /// only binding frames shadow, because cppnix resolves `builtins` out of
    /// its static base environment, so an enclosing `with` does not capture it
    /// on either arm.
    fn builtins_is_the_global(&self) -> bool {
        !self.scopes.iter().any(|frame| match frame {
            ScopeFrame::Bindings(names) => names.iter().any(|n| n == "builtins"),
            ScopeFrame::With => false,
        })
    }

    /// `builtins.<name>` as one op, when that is exactly what it means.
    ///
    /// Without this the compiler emits `BuiltinsSet` for the receiver, so
    /// every syntactic occurrence rebuilt a ~160-entry attrset at run time and
    /// selected one attribute out of it; `builtins.stringLength` in a loop
    /// cost 14.5us a reference against 0.06us for cppnix (ENG-12539). The op
    /// this emits is the one `Op::Builtin` already means, and the value it
    /// pushes is the value the set's slot holds, so nothing downstream can
    /// tell the two apart.
    ///
    /// Deliberately narrow. It fires only for an unshadowed global receiver, a
    /// statically-named first attribute that `set_member_index` says is a
    /// plain primop, and no `or` default -- because a default changes the
    /// select into a soft one whose miss has to reach `OrDefault`, and a name
    /// that is not a plain primop has to keep coming out of the set so that
    /// `attribute missing`, `unimplemented` and the constants all still say
    /// what they said.
    fn builtins_member_op(&self, base: &ast::Expr, attr: &ast::Attr) -> Option<Op> {
        let ast::Expr::Ident(id) = base else {
            return None;
        };
        if id.ident_token()?.text() != "builtins" || !self.builtins_is_the_global() {
            return None;
        }
        let name = static_attr_name(attr).ok().flatten()?;
        let idx = builtins::set_member_index(self.settings, &name)?;
        Some(Op::Builtin { idx })
    }

    fn compile_select(&mut self, sel: &ast::Select, ops: &mut Emit) -> Result<()> {
        let base = sel
            .expr()
            .ok_or_else(|| CompileError::Parse("select without base".into()))?;
        let path = sel
            .attrpath()
            .ok_or_else(|| CompileError::Parse("select without attrpath".into()))?;
        let default = sel.default_expr();
        let mut attrs: Vec<ast::Attr> = path.attrs().collect();
        let guarded = default.is_some();
        // `builtins.foo` resolved here stands in for the receiver and the
        // first select together, so both are dropped from what follows;
        // `builtins.foo.bar` keeps compiling `.bar` against the result, and
        // gets the same "expected a set" it always did.
        let folded = match (guarded, attrs.first()) {
            (false, Some(first)) => self.builtins_member_op(&base, first),
            _ => None,
        };
        match folded {
            Some(op) => {
                ops.push(op);
                attrs.remove(0);
            }
            None => self.compile(&base, ops)?,
        }
        for attr in &attrs {
            // With an `or` default, every step selects softly and a miss at
            // any depth reaches OrDefault as the marker; without one, a miss
            // throws at the step that failed, as in cppnix.
            match static_attr_name(attr)? {
                Some(name) => {
                    let sym = self.intern(&name);
                    ops.push(if guarded {
                        Op::SelectSoft { sym }
                    } else {
                        Op::Select { sym }
                    });
                }
                None => {
                    self.compile_attr_dynamic(attr, ops)?;
                    ops.push(if guarded {
                        Op::SelectSoftDyn
                    } else {
                        Op::SelectDyn
                    });
                }
            }
        }
        if let Some(d) = default {
            let t = self.compile_thunk(&d)?;
            ops.push(t);
            ops.push(Op::OrDefault);
        }
        Ok(())
    }

    fn compile_hasattr(&mut self, ha: &ast::HasAttr, ops: &mut Emit) -> Result<()> {
        let base = ha
            .expr()
            .ok_or_else(|| CompileError::Parse("? without base".into()))?;
        let path = ha
            .attrpath()
            .ok_or_else(|| CompileError::Parse("? without attrpath".into()))?;
        self.compile(&base, ops)?;
        let attrs: Vec<ast::Attr> = path.attrs().collect();
        let n = attrs.len();
        for (i, attr) in attrs.iter().enumerate() {
            let last = i + 1 == n;
            match static_attr_name(attr)? {
                Some(name) => {
                    let sym = self.intern(&name);
                    if last {
                        ops.push(Op::HasAttr { sym });
                    } else {
                        ops.push(Op::SelectSoft { sym });
                    }
                }
                None => {
                    self.compile_attr_dynamic(attr, ops)?;
                    if last {
                        ops.push(Op::HasAttrDyn);
                    } else {
                        ops.push(Op::SelectSoftDyn);
                    }
                }
            }
        }
        Ok(())
    }

    fn compile_with(&mut self, w: &ast::With, ops: &mut Emit) -> Result<()> {
        let ns = w
            .namespace()
            .ok_or_else(|| CompileError::Parse("with without namespace".into()))?;
        let body = w
            .body()
            .ok_or_else(|| CompileError::Parse("with without body".into()))?;
        let t = self.compile_thunk(&ns)?;
        ops.push(t);
        ops.push(Op::PushWith);
        self.scopes.push(ScopeFrame::With);
        let r = self.compile(&body, ops);
        self.scopes.pop();
        r?;
        ops.push(Op::PopEnv);
        Ok(())
    }

    fn compile_assert(&mut self, a: &ast::Assert, ops: &mut Emit) -> Result<()> {
        let cond = a
            .condition()
            .ok_or_else(|| CompileError::Parse("assert without condition".into()))?;
        let body = a
            .body()
            .ok_or_else(|| CompileError::Parse("assert without body".into()))?;
        self.compile(&cond, ops)?;
        ops.push(Op::Assert);
        self.compile(&body, ops)
    }
}

/// Turn the next `Module::units` offset into an IR index without entering the
/// `AttrOrigin` tag range. Those values are interpreted as slab coordinates by
/// `Clone` and `Drop`, so compiling a real unit with one would corrupt origin
/// ownership rather than merely make an oversized module.
fn checked_unit_index(index: usize) -> Result<u32> {
    let index = u32::try_from(index)
        .map_err(|_| CompileError::Eval("module has too many code units".to_owned()))?;
    if index >= crate::value2::AttrOrigin::PROJECTED_UNIT {
        return Err(CompileError::Eval(
            "module has too many code units for attribute provenance".to_owned(),
        ));
    }
    Ok(index)
}

/// A real instruction index must stay below `AttrOrigin::FORMALS`, which uses
/// `u32::MAX` in the `ip` word for a non-instruction source site.
fn checked_op_count(count: usize) -> Result<()> {
    let first_invalid = usize::try_from(crate::value2::AttrOrigin::FORMALS).unwrap_or(usize::MAX);
    if count > first_invalid {
        return Err(CompileError::Eval(
            "code unit has too many instructions for attribute provenance".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod origin_index_tests {
    use super::{checked_op_count, checked_unit_index};
    use crate::value2::AttrOrigin;

    /// Exercise the numeric boundary directly. Allocating four billion units
    /// would hide this invariant behind the allocator and never reach the cast
    /// whose collision this test exists to prevent.
    #[test]
    fn compiled_indices_stop_before_attribute_origin_sentinels() {
        let first_reserved = usize::try_from(AttrOrigin::PROJECTED_UNIT)
            .expect("u32 fits usize on supported targets");
        let Ok(last_real) = checked_unit_index(first_reserved - 1) else {
            unreachable!("the unit below the reserved range was rejected")
        };
        assert_eq!(last_real, AttrOrigin::PROJECTED_UNIT - 1);
        for reserved in [
            AttrOrigin::PROJECTED_UNIT,
            AttrOrigin::LIST_TO_ATTRS_UNIT,
            AttrOrigin::DYNAMIC_UNIT,
        ] {
            assert!(
                checked_unit_index(reserved as usize).is_err(),
                "reserved unit tag {reserved} compiled as a real unit"
            );
        }

        let formals =
            usize::try_from(AttrOrigin::FORMALS).expect("u32 fits usize on supported targets");
        assert!(checked_op_count(formals).is_ok());
        #[cfg(target_pointer_width = "64")]
        assert!(
            checked_op_count(formals + 1).is_err(),
            "a real instruction index can alias the FORMALS sentinel"
        );
    }
}

/// What one binding name is bound to. A `Node` is an attribute set under
/// construction: `a.b = 1; a.c = 2;` is one `a`, and so is
/// `a = { b = 1; }; a = { c = 2; };`, because cppnix's parser merges two
/// bindings of one name whenever both values are set literals.
enum BindTree {
    Leaf(Expr),
    Node(SetBuild),
}

/// An attribute set being assembled from one or more sources.
struct SetBuild {
    /// From the FIRST literal to occupy this name. cppnix keeps that one's
    /// `rec` and discards any on a set merged in later, so the earlier set's
    /// scope ends up covering the later one's attributes -- NixOS/nix#9020,
    /// which the corpus calls regrettable and pins anyway.
    rec: bool,
    /// Byte offset of the `{` (or of the `let`) this set is assembled from,
    /// used as the position of the ops that build it.
    pos: u32,
    /// `(name, byte offset of the name token, value)`. The offset is the
    /// FIRST component of the attrpath that introduced the name, which is
    /// where cppnix records it too: `{ a.b = 1; }` desugars to
    /// `{ a = { b = 1; }; }` and `unsafeGetAttrPos "a"` answers the position
    /// of `a`.
    kids: Vec<(String, u32, BindTree)>,
    /// Names known only at run time; never part of a `rec` scope. The value
    /// is a `BindTree` and not an `Expr` because a dynamic component can have
    /// more path after it: `{ ${a}.b = 1; }` binds one run-time name to a set
    /// that this compiler builds, and there is no source expression for that
    /// set to point at.
    dynamic: Vec<(ast::Attr, u32, BindTree)>,
    inherits: Vec<ast::Inherit>,
}

/// `pos` defaults to [`NO_POS`] rather than to `0`, which `#[derive(Default)]`
/// would give it and which is a real byte offset -- the first character of the
/// file. An implicit set built for `{ a.b = 1; }`'s `a` has no `{` of its own,
/// and reporting the top of the file for it would be worse than reporting
/// nothing.
impl Default for SetBuild {
    fn default() -> Self {
        SetBuild {
            rec: false,
            pos: NO_POS,
            kids: Vec::new(),
            dynamic: Vec::new(),
            inherits: Vec::new(),
        }
    }
}

/// A set literal's own entries, as a `SetBuild`.
fn absorb(a: &ast::AttrSet) -> Result<SetBuild> {
    build_entries(a, a.rec_token().is_some(), offset_of(a.syntax()))
}

/// `let`, `let { }` and `rec { }` all assemble the same shape.
fn build_entries(a: &impl HasEntry, rec: bool, pos: u32) -> Result<SetBuild> {
    let mut b = SetBuild {
        rec,
        pos,
        ..SetBuild::default()
    };
    merge_literal(&mut b, a)?;
    Ok(b)
}

/// Fold one set literal's entries into `b`. The literal's own `rec` is NOT
/// consulted: only the first set to claim a name decides that.
fn merge_literal(b: &mut SetBuild, a: &impl HasEntry) -> Result<()> {
    b.inherits.extend(a.inherits());
    for kv in a.attrpath_values() {
        let path = kv
            .attrpath()
            .ok_or_else(|| CompileError::Parse("attr without name".into()))?;
        let value = kv
            .value()
            .ok_or_else(|| CompileError::Parse("attr without value".into()))?;
        let attrs: Vec<ast::Attr> = path.attrs().collect();
        // Every component of one binding gets the position of the whole
        // attrpath, which is what cppnix's parser passes to `addAttr`
        // (`state->at(@$)` over the `attrpath '=' expr ';'` rule). So
        // `{ a.b = 1; }` answers the `a` for both `a` and `b`, and not the
        // `b` for `b`. Verified against nix-instantiate 2026-08-06: both
        // answer column 34 in `builtins.unsafeGetAttrPos "_" ({ a.b = 1; })`.
        let root_pos = attrs.first().map_or(NO_POS, attr_offset);
        tree_insert(b, &attrs, value, root_pos)?;
    }
    Ok(())
}

/// Insert one attrpath, following cppnix's addAttr: descend through existing
/// set literals, create implicit sets for missing levels, and merge at the
/// leaf when both sides are sets. Anything else is a duplicate.
///
/// A component whose name is only known at run time stops the descent. It
/// cannot merge with anything, because nothing here can tell whether it will
/// collide, so it becomes its own entry and everything after it becomes a
/// fresh set hanging off it. That is also why the recursion below builds into
/// a `SetBuild::default()` rather than looking for one to extend.
fn tree_insert(b: &mut SetBuild, path: &[ast::Attr], value: Expr, root_pos: u32) -> Result<()> {
    let Some((head_attr, rest)) = path.split_first() else {
        return Err(CompileError::Parse("empty attrpath".into()));
    };
    let head = &match static_attr_name(head_attr)? {
        Some(name) => name,
        None => {
            if rest.is_empty() {
                b.dynamic
                    .push((head_attr.clone(), root_pos, BindTree::Leaf(value)));
            } else {
                let mut sub = SetBuild {
                    pos: attr_offset(head_attr),
                    ..SetBuild::default()
                };
                tree_insert(&mut sub, rest, value, root_pos)?;
                b.dynamic
                    .push((head_attr.clone(), root_pos, BindTree::Node(sub)));
            }
            return Ok(());
        }
    };
    let head_pos = root_pos;
    let existing = b.kids.iter().position(|(n, _, _)| n == head);
    if rest.is_empty() {
        let Some(i) = existing else {
            b.kids.push((head.clone(), head_pos, BindTree::Leaf(value)));
            return Ok(());
        };
        // Both sides sets: merge. Otherwise it is a redefinition.
        let Expr::AttrSet(incoming) = &value else {
            return Err(CompileError::Parse(format!(
                "attribute '{head}' already defined"
            )));
        };
        let slot = b
            .kids
            .get_mut(i)
            .ok_or_else(|| CompileError::Parse("internal: lost tree slot".into()))?;
        promote(slot, head)?;
        let (_, _, BindTree::Node(node)) = slot else {
            return Err(CompileError::Parse("internal: promotion failed".into()));
        };
        return merge_literal(node, incoming);
    }
    let idx = match existing {
        Some(i) => i,
        None => {
            b.kids.push((
                head.clone(),
                head_pos,
                BindTree::Node(SetBuild {
                    pos: head_pos,
                    ..SetBuild::default()
                }),
            ));
            b.kids.len() - 1
        }
    };
    let slot = b
        .kids
        .get_mut(idx)
        .ok_or_else(|| CompileError::Parse("internal: lost tree slot".into()))?;
    promote(slot, head)?;
    match slot {
        (_, _, BindTree::Node(node)) => tree_insert(node, rest, value, root_pos),
        _ => Err(CompileError::Parse("internal: promotion failed".into())),
    }
}

/// Turn a `Leaf` holding a set literal into the `Node` that can absorb more
/// attributes. A leaf holding anything else is a duplicate definition.
fn promote(slot: &mut (String, u32, BindTree), name: &str) -> Result<()> {
    if let (_, _, BindTree::Leaf(Expr::AttrSet(a))) = slot {
        let built = absorb(a)?;
        slot.2 = BindTree::Node(built);
        return Ok(());
    }
    if matches!(slot.2, BindTree::Node(_)) {
        return Ok(());
    }
    Err(CompileError::Parse(format!(
        "attribute '{name}' already defined"
    )))
}

/// The attr name when the parser can fold it to a constant, `None` when it
/// is only known at run time.
///
/// `None` rather than an error, because a run-time name is not a failure
/// anywhere this is called: `{ ${e} = 1; }`, `x.${e}` and `x ? ${e}` all
/// compile, through the `SelectDyn`/`HasAttrDyn`/`MkAttrs` family. The two
/// positions where cppnix forbids one -- `let` and `inherit` -- say so
/// themselves, with cppnix's own parse error, rather than reading a refusal
/// out of this function's `Err` arm. Before ENG-12546's burndown they did
/// read one, and `let ${e} = ...` was already the parse error while
/// `inherit ${e}` was a refusal, so one construct produced two different
/// answers to the same question.
///
/// The `Err` arm is now only a malformed tree.
fn static_attr_name(attr: &ast::Attr) -> Result<Option<String>> {
    /// A string node's text when it has no interpolation in it.
    ///
    /// `parser-state.hh:91`'s `AttrName::visit`: a `string_attr` whose
    /// underlying expression is an `ExprString` takes the `std::string_view`
    /// overload, so `"a"` and `${"a"}` are both static names. Anything that
    /// survived constant folding as some other `Expr *` is dynamic.
    fn folded(s: &ast::Str) -> Option<String> {
        match s.normalized_parts().as_slice() {
            [] => Some(String::new()),
            [ast::InterpolPart::Literal(text)] => Some(text.clone()),
            _ => None,
        }
    }
    match attr {
        ast::Attr::Ident(i) => Ok(Some(
            i.ident_token()
                .ok_or_else(|| CompileError::Parse("attr ident without token".into()))?
                .text()
                .to_string(),
        )),
        ast::Attr::Str(s) => Ok(folded(s)),
        ast::Attr::Dynamic(d) => Ok(match d.expr() {
            Some(Expr::Str(s)) => folded(&s),
            _ => None,
        }),
    }
}

/// cppnix's `attrs` rule (`parser.y:529`) throws this the moment an
/// `inherit` names something the parser could not fold to a symbol, so it
/// fires whether or not the binding is ever demanded.
fn no_dynamic_in_inherit() -> CompileError {
    CompileError::Parse("dynamic attributes not allowed in inherit".into())
}

/// The `u16` a set-building op carries for one of its attribute tables, from
/// the table's actual length, or a compile error: `MkAttrs` is zipped with
/// its attr site by position, so the count and the table must not disagree.
fn attr_count(len: usize) -> Result<u16> {
    u16::try_from(len).map_err(|_| CompileError::Parse("too many attributes in one set".into()))
}

fn node_name(e: &Expr) -> &'static str {
    match e {
        Expr::Apply(_) => "function application",
        Expr::Assert(_) => "assert",
        Expr::AttrSet(_) => "attribute set",
        Expr::BinOp(_) => "binary operator",
        Expr::Error(_) => "parse error node",
        Expr::HasAttr(_) => "has-attr",
        Expr::Ident(_) => "identifier",
        Expr::IfElse(_) => "if",
        Expr::Lambda(_) => "lambda",
        Expr::LegacyLet(_) => "legacy let",
        Expr::LetIn(_) => "let",
        Expr::List(_) => "list",
        Expr::Literal(_) => "literal",
        Expr::Paren(_) => "parentheses",
        Expr::Path(_) => "path literal",
        Expr::Root(_) => "root",
        Expr::Select(_) => "attribute selection",
        Expr::Str(_) => "string",
        Expr::UnaryOp(_) => "unary operator",
        Expr::With(_) => "with",
    }
}

#[cfg(test)]
mod span_tests {
    use super::compile_source;
    use crate::compile::Origin;
    use crate::ir::{AttrSite, Module, NO_POS};

    /// Enough of the language that a construct emitting unattributed ops is
    /// likely to be in here somewhere.
    const CORPUS: &[&str] = &[
        "1 + 2 * 3 - 4 / 5",
        r#""a" + "b" + "${"c"}""#,
        "[ 1 2 3 ] ++ [ 4 ]",
        "{ a = 1; b.c = 2; ${\"d\"} = 3; }",
        "rec { a = 1; b = a + 1; }",
        "let a = 1; b = a; in { inherit a b; }",
        "let s = { x = 1; }; in { inherit (s) x; }",
        "x: y: x y",
        "{ a, b ? 1, ... }@args: a + b",
        "if true then 1 else 2",
        "assert 1 == 1; 3",
        "with { a = 1; }; a",
        "let f = n: if n == 0 then 0 else f (n - 1); in f 3",
        "builtins.map (x: x + 1) [ 1 2 ]",
        "builtins.foldl' (a: b: a + b) 0 [ 1 2 3 ]",
        "(x: x) 1",
        "{ a = 1; } // { b = 2; }",
        "[ 1 ] == [ 1 ] && !(1 < 2) || false",
        "\"a\" ? b",
        "{ a.b = 1; }.a.b or 2",
        "''\n  indented\n''",
        "./relative/path",
        "let x = 1; in x.y.z or (throw \"no\")",
        "builtins.tryEval (throw \"no\")",
        "builtins.genList (i: i) 3",
        "-1",
        "1.5 + 2.5",
        "builtins.toString 1",
    ];

    fn modules() -> Vec<(&'static str, Module)> {
        CORPUS
            .iter()
            .filter_map(|src| {
                let m = compile_source(src, "/", Origin::String, &crate::eval::Settings::default());
                assert!(m.is_ok(), "{src} did not compile: {m:?}");
                m.ok().map(|m| (*src, m))
            })
            .collect()
    }

    /// The span table is parallel to `ops` by construction, and a short one
    /// silently reports [`NO_POS`] for the tail of every unit rather than
    /// failing, so nothing else would notice.
    #[test]
    fn the_span_table_is_the_same_length_as_the_ops() {
        for (src, m) in modules() {
            for (i, unit) in m.units.iter().enumerate() {
                assert_eq!(
                    unit.spans.len(),
                    unit.ops.len(),
                    "unit {i} of `{src}` has {} ops and {} spans",
                    unit.ops.len(),
                    unit.spans.len()
                );
            }
        }
    }

    /// Every span is either [`NO_POS`] or an offset that lands inside the
    /// source. An offset past the end resolves to a plausible-looking line
    /// and column that names nothing, which is worse than no position.
    #[test]
    fn every_span_is_inside_its_source() {
        for (src, m) in modules() {
            let len = u32::try_from(src.len()).unwrap_or(u32::MAX);
            for unit in &m.units {
                for &span in &unit.spans {
                    assert!(
                        span == NO_POS || span < len,
                        "`{src}` ({len} bytes) has a span at {span}"
                    );
                }
                for site in &unit.attr_sites {
                    for &(_, offset) in &site.names {
                        assert!(
                            offset == NO_POS || offset < len,
                            "`{src}` has an attr site at {offset}"
                        );
                    }
                }
            }
        }
    }

    /// Every op except the synthesised `Ret` carries a position.
    ///
    /// Measured, not aspired to: over the corpus above, 331 ops carry 250
    /// positions and every one of the 81 without is a `Ret`, one per unit.
    /// `Ret` has no token -- the compiler appends it after the last thing the
    /// user wrote -- and it cannot fail, so it has nothing to report.
    ///
    /// What this CANNOT catch, checked by breaking it: `Emit::at` is a
    /// cursor, so an attribution that goes missing leaves the previous
    /// construct's offset in place rather than [`NO_POS`]. Making
    /// `operator_offset` return `None` still passes here and fails
    /// `tests/positions.rs`, which pins exact columns against cppnix. This
    /// test covers the cursor being absent; that one covers it being wrong,
    /// and neither substitutes for the other.
    #[test]
    fn every_op_but_the_synthesised_return_carries_a_position() {
        let mut total = 0usize;
        for (src, m) in modules() {
            for unit in &m.units {
                total += unit.spans.len();
                for (op, &span) in unit.ops.iter().zip(unit.spans.iter()) {
                    assert!(
                        span != NO_POS || op.kind() == crate::ir::OpKind::Ret,
                        "`{src}` emitted {op:?} with no position"
                    );
                }
            }
        }
        assert!(total > 250, "the corpus shrank to {total} ops");
    }

    /// The compiler<->VM contract on attribute sites is one function,
    /// `CodeUnit::check_attr_sites` (the module-cache decoder runs the same
    /// one on every cached unit), so the corpus is checked against it rather
    /// than against a second transcription of the rules.
    #[test]
    fn the_attr_site_table_matches_its_ops() {
        for (src, m) in modules() {
            for unit in &m.units {
                if let Err(detail) = unit.check_attr_sites(&m.symbols) {
                    panic!("attr sites of `{src}`: {detail}");
                }
            }
        }
    }

    /// Static names sit in the site in emission order: every `inherit`
    /// first, then the bindings, each in source order. The VM pairs them with
    /// the popped values by position, so this order IS the meaning of the
    /// op; a sorted table would name every attribute wrong.
    #[test]
    fn static_attr_names_are_recorded_in_emission_order() {
        let module = five_name_module();
        let site = five_name_site(&module);
        let want = FIVE_NAMES_IN_EMISSION_ORDER;
        assert_eq!(site.static_names().len(), want.len());
        for (i, (&(sym, _), name)) in site.static_names().iter().zip(want).enumerate() {
            assert!(
                module.symbol_is(sym, name),
                "static name {i} of the fixture is not `{name}`"
            );
        }
    }

    /// A position lookup arrives with text and goes through the site's text
    /// index, not a scan of the emission-ordered table: every static name
    /// resolves to its own position and an absent name to `None`.
    #[test]
    fn static_attr_positions_resolve_through_the_text_index() {
        let module = five_name_module();
        let site = five_name_site(&module);
        for (i, name) in FIVE_NAMES_IN_EMISSION_ORDER.iter().enumerate() {
            let (_, offset) = site.static_names()[i];
            assert_ne!(offset, NO_POS, "`{name}` has a position");
            assert_eq!(
                site.static_offset(name, &module.symbols),
                Some(offset),
                "`{name}` resolves to its own position"
            );
        }
        assert_eq!(site.static_offset("absent", &module.symbols), None);
        assert_eq!(site.static_offset("", &module.symbols), None);
    }

    /// `{ z = 3; inherit (e) q p; a = 4; m = 5; }`: the static names in the
    /// order the op pops them (inherits first, then bindings, source order).
    const FIVE_NAMES_IN_EMISSION_ORDER: [&str; 5] = ["q", "p", "z", "a", "m"];

    fn five_name_module() -> Module {
        let src = "let e = { p = 1; q = 2; }; in { z = 3; inherit (e) q p; a = 4; m = 5; }";
        compile_source(src, "/", Origin::String, &crate::eval::Settings::default())
            .expect("fixture compiles")
    }

    fn five_name_site(module: &Module) -> &AttrSite {
        module
            .units
            .iter()
            .flat_map(|unit| {
                unit.attr_sites.iter().filter(move |site| {
                    unit.ops.get(site.ip as usize).is_some_and(|op| {
                        matches!(
                            op,
                            crate::ir::Op::MkAttrs {
                                statics: 5,
                                dynamics: 0
                            }
                        )
                    })
                })
            })
            .next()
            .expect("fixture emitted the five-name MkAttrs site")
    }

    /// A runtime-name site keeps static symbols in the compiled table and
    /// records pair indices only for runtime-computed names. This is what
    /// keeps one dynamic binding from becoming per-execution work for every
    /// static binding in the same set.
    #[test]
    fn a_dynamic_attr_site_separates_static_and_runtime_names() {
        let src = r#"let k = "b"; in { a = 1; ${k} = 2; }"#;
        let pos = |needle: &str| {
            u32::try_from(src.find(needle).expect("the fixture moved"))
                .expect("the fixture exceeds u32")
        };
        let module = compile_source(src, "/", Origin::String, &crate::eval::Settings::default())
            .expect("fixture compiles");
        let site = module
            .units
            .iter()
            .flat_map(|unit| {
                unit.attr_sites.iter().filter(move |site| {
                    unit.ops.get(site.ip as usize).is_some_and(
                        |op| matches!(op, crate::ir::Op::MkAttrs { dynamics, .. } if *dynamics > 0),
                    )
                })
            })
            .next()
            .expect("fixture emitted a dynamic MkAttrs site");
        assert_eq!(site.static_names(), [(0, pos("a = 1"))]);
        assert_eq!(site.dynamic_names(), [(0, pos("${k}"))]);
    }

    #[test]
    fn one_dynamic_binding_does_not_make_static_bindings_runtime_entries() {
        use std::fmt::Write as _;

        let mut src = "let k = \"dynamic\"; in {".to_owned();
        for i in 0..1_000 {
            write!(&mut src, " static{i:04} = {i};").expect("writing to a string succeeds");
        }
        src.push_str(" ${k} = 1; }");
        let module = compile_source(&src, "/", Origin::String, &crate::eval::Settings::default())
            .expect("fixture compiles");
        let site = module
            .units
            .iter()
            .flat_map(|unit| {
                unit.attr_sites.iter().filter(move |site| {
                    unit.ops.get(site.ip as usize).is_some_and(
                        |op| matches!(op, crate::ir::Op::MkAttrs { dynamics, .. } if *dynamics > 0),
                    )
                })
            })
            .next()
            .expect("fixture emitted a dynamic MkAttrs site");

        assert_eq!(site.static_names().len(), 1_000);
        assert_eq!(site.dynamic_names().len(), 1);
        assert_eq!(site.dynamic_names()[0].0, 0);
    }
}

#[cfg(test)]
mod builtins_fold_tests {
    use super::compile_source;
    use crate::builtins;
    use crate::ir::Op;

    /// The ops of the entry unit, or empty when the source does not compile
    /// (the assertion is what reports that; the workspace denies `panic`).
    fn entry_ops(src: &str) -> Vec<Op> {
        let module = compile_source(
            src,
            "/",
            crate::compile::Origin::String,
            &crate::eval::Settings::default(),
        );
        assert!(module.is_ok(), "{src} did not compile: {module:?}");
        let Ok(module) = module else {
            return Vec::new();
        };
        module
            .units
            .get(module.entry as usize)
            .map(|unit| unit.ops.clone())
            .unwrap_or_default()
    }

    fn builds_the_set(src: &str) -> bool {
        entry_ops(src).contains(&Op::BuiltinsSet)
    }

    fn folds_to(src: &str, name: &str) -> bool {
        let Some(idx) = builtins::global_index(name) else {
            return false;
        };
        entry_ops(src).contains(&Op::Builtin { idx })
    }

    /// The point of the change: a `builtins.<name>` reference carries no
    /// instruction that builds the set.
    #[test]
    fn a_named_builtin_does_not_build_the_set() {
        assert!(!builds_the_set(r#"builtins.stringLength "abc""#));
        assert!(folds_to(r#"builtins.stringLength "abc""#, "stringLength"));
        // Including where it is the receiver of a further select, which keeps
        // compiling against the folded value.
        assert!(!builds_the_set("builtins.stringLength.x"));
    }

    /// First-class `builtins` still builds it, because there is nothing else
    /// it could mean.
    #[test]
    fn first_class_builtins_still_builds_the_set() {
        assert!(builds_the_set("builtins"));
        assert!(builds_the_set("builtins ? stringLength"));
        // A name only the runtime knows. (A literal `${"stringLength"}` is
        // not one of these: the compiler folds it to a static name, as it did
        // before this change.)
        assert!(builds_the_set(
            r#"let n = "stringLength"; in builtins.${n} "abc""#
        ));
        assert!(folds_to(
            r#"builtins.${"stringLength"} "abc""#,
            "stringLength"
        ));
    }

    /// A local binding shadows the global, on this arm as on cppnix's
    /// (`let builtins = { stringLength = _: 99; }; in builtins.stringLength "abc"`
    /// answers 99 under nix 2.34.7).
    #[test]
    fn a_local_named_builtins_shadows_the_global() {
        let src = r#"let builtins = { stringLength = _: 99; }; in builtins.stringLength "abc""#;
        assert!(!builds_the_set(src));
        assert!(!folds_to(src, "stringLength"));
        // Every shape that pushes a binding frame, not just `let`: a plain
        // lambda parameter, a destructured one, and a rec set.
        let lambda = r#"builtins: builtins.stringLength "abc""#;
        assert!(!folds_to(lambda, "stringLength"));
        let formals = r#"{ builtins }: builtins.stringLength "abc""#;
        assert!(!folds_to(formals, "stringLength"));
        let rec =
            r#"rec { builtins = { stringLength = _: 99; }; x = builtins.stringLength "abc"; }"#;
        assert!(!folds_to(rec, "stringLength"));
    }

    /// A `with` does not, for the same reason cppnix's does not: `builtins`
    /// lives in the static base environment, which the with-scope sits under
    /// rather than over. Verified against nix 2.34.7, which answers 3 for
    /// `with { builtins = { stringLength = _: 99; }; }; builtins.stringLength "abc"`.
    #[test]
    fn a_with_scope_does_not_shadow_the_global() {
        let src = r#"with { builtins = { stringLength = _: 99; }; }; builtins.stringLength "abc""#;
        assert!(folds_to(src, "stringLength"));
    }

    /// The members that are not plain primops keep coming out of the set, so
    /// the constants, the derivation wrapper, the unimplemented slots and
    /// `attribute missing` all keep saying what they said.
    #[test]
    fn non_primop_members_and_misses_still_build_the_set() {
        assert!(builds_the_set("builtins.langVersion"));
        assert!(builds_the_set("builtins.derivation"));
        assert!(builds_the_set("builtins.fetchMercurial"));
        assert!(builds_the_set("builtins.nope"));
        // An `or` default turns the select soft, and a soft miss has to reach
        // `OrDefault` rather than being decided at compile time.
        assert!(builds_the_set("builtins.stringLength or 7"));
    }
}

/// The constant and symbol pools are indexed by a hash map (ENG-12860), and
/// these are the two ways that can go wrong that a compile still survives.
///
/// A miss that should have hit only wastes a pool slot. A *hit that should
/// have missed* compiles the wrong value into the program, silently, so it
/// gets a direct test rather than an argument that `Hash` and `Eq` agree.
#[cfg(test)]
mod pool_index_tests {
    use super::compile_source;
    use crate::compile::Origin;
    use crate::ir::{Const, Module};

    fn module_of(src: &str) -> Module {
        let m = compile_source(src, "/", Origin::String, &crate::eval::Settings::default());
        assert!(m.is_ok(), "{src} did not compile: {m:?}");
        m.unwrap_or_default()
    }

    fn count_const(m: &Module, c: &Const) -> usize {
        m.consts.iter().filter(|x| *x == c).count()
    }

    /// Count by *variant and payload*, never by `==`.
    ///
    /// The first draft of the string-versus-path test below counted with
    /// `==` and did not fail when `Const`'s `eq` was broken to merge the two
    /// variants: the collapsed pool holds one entry that compares equal to
    /// both probes, so both counts read 1 and the assertion held. A test for
    /// an equivalence relation cannot be written in terms of that relation.
    fn count_str(m: &Module, want: &str) -> usize {
        m.consts
            .iter()
            .filter(|c| matches!(c, Const::Str(s) if s == want))
            .count()
    }

    fn count_path(m: &Module, want: &str) -> usize {
        m.consts
            .iter()
            .filter(|c| matches!(c, Const::Path(p) if p.path.as_ref() == want))
            .count()
    }

    /// `"/tmp/x"` and `/tmp/x` share a payload and are different constants:
    /// one pushes a string, the other a path, and a path is what a store copy
    /// keys on. Hashing the payload without the discriminant would collapse
    /// them onto one slot and hand the VM the wrong variant -- a Tier 1
    /// difference reachable from two adjacent lines of Nix.
    #[test]
    fn a_string_and_a_path_with_the_same_text_get_their_own_slots() {
        let m = module_of("{ a = \"/tmp/x\"; b = /tmp/x; }");
        assert_eq!(
            (count_str(&m, "/tmp/x"), count_path(&m, "/tmp/x")),
            (1, 1),
            "pool: {:?}",
            m.consts
        );
    }

    #[test]
    fn a_relative_literal_keeps_the_imported_modules_mount() {
        let mount = "/nix/store/00000000000000000000000000000000-source";
        let file = format!("{mount}/sub/module.nix");
        let module = compile_source(
            "./a",
            &format!("{mount}/sub"),
            Origin::MountedFile {
                path: &file,
                mount_point: mount,
            },
            &crate::eval::Settings::default(),
        )
        .unwrap_or_default();
        assert!(module.consts.iter().any(|constant| matches!(
            constant,
            Const::Path(p)
                if matches!(&p.root, crate::value2::Root::Mounted(root) if root.as_ref() == mount)
                    && p.path.as_ref() == format!("{mount}/sub/a")
        )));
    }

    /// Deduplication is what the index replaced, so it has to still happen.
    /// An index that never hit would compile correctly and grow the pool
    /// without bound, and nothing else in the suite would notice.
    #[test]
    fn a_repeated_constant_takes_one_slot() {
        let m = module_of("[ \"dup\" \"dup\" \"dup\" 7 7 ]");
        assert_eq!(
            count_const(&m, &Const::Str("dup".into())),
            1,
            "{:?}",
            m.consts
        );
        assert_eq!(count_const(&m, &Const::Int(7)), 1, "{:?}", m.consts);
    }

    /// The same property for the symbol pool, whose scan had the identical
    /// shape and 148,888,726 comparisons on the same evaluation.
    ///
    /// The source is chosen for what it *interns*, which is not what a
    /// reader expects: a plain attrset key is a constant, and only lambda
    /// parameters, `inherit` names and static select paths reach
    /// [`Compiler::intern`]. A source of attrset literals leaves `symbols`
    /// empty, and every assertion below then holds over nothing -- which is
    /// how the first draft of this test passed while testing the pool it
    /// names not at all. Hence the emptiness check before the loop.
    #[test]
    fn a_repeated_symbol_takes_one_slot() {
        let m = module_of(
            "let s = { a = 1; b = 2; }; in {
                f = { a, b }: a + b;
                g = { a, b }: b + a;
                h = s.a + s.b;
                inherit (s) a b;
            }",
        );
        assert!(
            !m.symbols.is_empty(),
            "nothing was interned; the source tests nothing"
        );
        for name in ["a", "b"] {
            assert_eq!(
                m.symbols.iter().filter(|s| s.as_str() == name).count(),
                1,
                "symbol '{name}' duplicated in {:?}",
                m.symbols
            );
        }
    }

    /// No pool entry may repeat, whatever the source. Two equal entries mean
    /// a lookup missed one already there, and the pool is what `Op::Const`
    /// indexes, so a drifting pool is a drifting program.
    #[test]
    fn no_pool_entry_is_ever_duplicated() {
        let src = "rec {
            xs = [ 1 2 3 1 2 3 \"a\" \"a\" true false null 1.5 1.5 ];
            ys = { p = ./rel; q = ./rel; r = \"str\"; s = \"str\"; };
            f = { p, q }: { inherit p q; z = ys.p + ys.q; };
            g = { p, q }: ys.p;
        }";
        let m = module_of(src);
        // Both pools have to be non-empty or the loops below hold vacuously.
        assert!(!m.consts.is_empty() && !m.symbols.is_empty(), "{m:?}");
        for (i, c) in m.consts.iter().enumerate() {
            assert_eq!(
                m.consts.iter().position(|x| x == c),
                Some(i),
                "const {c:?} appears twice in {:?}",
                m.consts
            );
        }
        for (i, s) in m.symbols.iter().enumerate() {
            assert_eq!(
                m.symbols.iter().position(|x| x == s),
                Some(i),
                "symbol {s:?} appears twice in {:?}",
                m.symbols
            );
        }
    }
}

#[cfg(test)]
mod immediate_value_tests {
    use super::compile_source;
    use crate::compile::Origin;
    use crate::ir::{Module, Op};

    fn module_of(src: &str) -> Module {
        let m = compile_source(src, "/", Origin::String, &crate::eval::Settings::default());
        assert!(m.is_ok(), "{src} did not compile: {m:?}");
        m.unwrap_or_default()
    }

    fn deferred_in_entry(m: &Module) -> usize {
        m.units[m.entry as usize]
            .ops
            .iter()
            .filter(|op| matches!(op, Op::Thunk { .. } | Op::Closure { .. }))
            .count()
    }

    /// `{ a = false; }`, `{ a = "s"; }`, `{ a = [ ]; }`: cppnix's `maybeThunk`
    /// returns the base environment's cell for a global and the value itself
    /// for a constant (`ExprVar`, `ExprInt`, `ExprFloat`, `ExprString`,
    /// `ExprPath`, `ExprList` when empty), so `a` is a value, not a thunk,
    /// and a reader that looks without forcing sees it. The entry then
    /// defers nothing.
    #[test]
    fn values_are_values_not_thunks() {
        for value in [
            "false",
            "true",
            "null",
            "builtins",
            "map",
            "\"s\"",
            "''s''",
            "1",
            "1.5",
            "./p",
            "[ ]",
            "(\"s\")",
            "(false)",
            "((true))",
            "(builtins)",
        ] {
            let m = module_of(&format!("{{ a = {value}; }}"));
            let deferred = deferred_in_entry(&m);
            assert_eq!(deferred, 0, "{value}: the entry deferred {deferred} cells");
        }
        // The let/rec fill ops push a constant the same way (cppnix's
        // `ExprLet::eval` and `ExprAttrs::eval` call `maybeThunk` on each
        // binding); a binding that runs code is still a thunk there.
        for (source, deferred) in [
            ("let z = \"x\"; in z", 0),
            ("let z = [ ]; in z", 0),
            ("let z = 1; y = ./p; in z", 0),
            ("let z = false; in z", 0),
            ("rec { flake = (false); }", 0),
            ("let false = (x: x) true; z = false; in z", 2),
            ("let z = (x: x) 1; in z", 1),
        ] {
            let m = module_of(source);
            assert_eq!(deferred_in_entry(&m), deferred, "{source}");
        }
    }
}
