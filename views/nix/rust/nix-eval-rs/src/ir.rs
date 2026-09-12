//! The bytecode IR: serializable, index-based, no pointers. A `Module` is
//! the unit of content addressing (one per source file); a `CodeUnit` is one
//! lambda body (or the top-level expression). Frontends other than Nix are
//! expected to emit this shape, so nothing in here references the rnix CST.
//!
//! Encoding choices carried from the design review (ENG-12068): fixed-width
//! ops rather than varints (varint decode showed up in snix profiles), a
//! const pool of pure literals only (runtime values in the pool are what
//! make snix Chunks unserializable), and `with`-scope ops as a feature a
//! frontend can simply not use.

/// One instruction. Operands index the const pool, the symbol table, the
/// unit list, or the locals of the current frame, per op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Push const pool entry.
    Const(u32),
    /// Push the value of the local at (depth, slot), forcing it.
    GetLocal {
        depth: u16,
        slot: u16,
    },
    /// Push the local's slot without forcing (rec-attrset construction).
    GetLocalLazy {
        depth: u16,
        slot: u16,
    },
    /// Push builtin `idx` as a value (unapplied).
    Builtin {
        idx: u16,
    },
    /// Push the `builtins` attrset.
    BuiltinsSet,
    /// Push the `derivation` global: a thunk over the wrapper source cppnix
    /// embeds, not a primop. See `Vm`'s `DERIVATION_INTERNAL` and cppnix's
    /// `EvalState::derivationInternal`.
    DerivationGlobal,
    /// Push `__nixPath`: the default search path, which the embedder answers
    /// through `NeedPath::NixPath`. Not a primop and not a constant either --
    /// cppnix precomputes the list into `staticBaseEnv`, and here it is a
    /// question so that a read set sees an evaluation depend on `-I`.
    NixPathGlobal,
    /// Push a thunk over unit `unit`, capturing the current environment.
    Thunk {
        unit: u32,
    },
    /// Push a closure over unit `unit` (a lambda), capturing the environment.
    Closure {
        unit: u32,
    },
    /// Call: pop argument, pop callee, push result.
    Apply,
    /// Pop `n` values into a fresh environment frame (for let/lambda bodies),
    /// lazily: values stay thunks until forced.
    PushEnv {
        n: u16,
    },
    PopEnv,
    /// Pop scrutinee; if false jump forward by `target` ops.
    JumpIfFalse {
        target: u32,
    },
    /// Unconditional forward jump.
    Jump {
        target: u32,
    },
    /// Arithmetic / comparison / logic on the top two stack values.
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    Neq,
    Lt,
    Leq,
    Gt,
    Geq,
    Not,
    Negate,
    /// String/path concatenation of the top `n` values (interpolation).
    ConcatStrings {
        n: u16,
    },
    /// List of the top `n` values (in push order).
    MkList {
        n: u16,
    },
    /// List concatenation (++) of the top two lists.
    ConcatLists,
    /// Attrset from the top `statics + 2 * dynamics` stack entries: `statics`
    /// values whose names are the static half of this op's [`AttrSite`]
    /// (module symbols in the same order, resolved through the link table),
    /// under `dynamics` (name, value) pairs whose names were pushed as
    /// strings.
    ///
    /// Static names never touch the stack and are never re-interned per
    /// build (the link table interns each module symbol once per VM, on the
    /// module's first use). Before this split every attribute of every set
    /// built pushed its name as a string constant and re-interned it on each
    /// build: 50.7M interns on one NixOS toplevel (dev-compute-4,
    /// 2026-09-04), the single largest source.
    MkAttrs {
        statics: u16,
        dynamics: u16,
    },
    /// Attrset update (//).
    Update,
    /// Attrset from the top 2*n stack entries laid over the set BELOW them:
    /// n (name, value) pairs added to an existing set, with the same
    /// duplicate check `MkAttrs` makes.
    ///
    /// It exists for `rec { __overrides = ...; ${e} = ...; }`, where cppnix
    /// applies the overrides first and only then adds the dynamic
    /// attributes, checking each against the post-override bindings
    /// (`eval.cc:1489`, "Dynamic attrs apply *after* rec and __overrides",
    /// and the `bindings.bindings->get(nameSym)` two lines below it).
    /// `MkAttrs` followed by `Update` cannot express that: it would let an
    /// override silently win a name cppnix rejects as already defined.
    MkAttrsOnto {
        n: u16,
    },
    /// Pop attrset, push value of attr `sym` (forcing the select), or throw.
    Select {
        sym: u32,
    },
    /// Select that pushes a miss marker instead of throwing when the base is
    /// not a set or lacks the attr. Feeds `or` defaults and `?` paths.
    SelectSoft {
        sym: u32,
    },
    SelectSoftDyn,
    /// Pop default thunk, then value-or-miss: pushes the default on miss,
    /// the value otherwise.
    OrDefault,
    /// Pop set-or-miss, push bool: has attribute (miss reads as false).
    HasAttr {
        sym: u32,
    },
    /// Dynamic select: pop name string, then set.
    SelectDyn,
    HasAttrDyn,
    /// Enter a `with` scope: pop the (lazy) subject onto the env chain.
    PushWith,
    /// Resolve an identifier that static scoping could not: search the
    /// with-stack at runtime.
    ResolveWith {
        sym: u32,
    },
    /// Call builtin by table index with the single argument on the stack.
    CallBuiltin {
        idx: u16,
    },
    /// Assert: pop condition; throw if false.
    Assert,
    Ret,
    /// Concatenation of the top `n` values where the first is a path, so the
    /// result is a path: cppnix's `ExprConcatStrings` with `forceString`
    /// off, which is what an interpolated path literal parses to
    /// (`parser.y`, `path_start string_parts_interpolated PATH_END`).
    ///
    /// A separate op rather than a flag on [`Op::ConcatStrings`] because the
    /// two differ in what they do to the world, not only in what they
    /// return. `forceString` is also cppnix's `copyToStore`, so a path part
    /// inside a *string* is copied into the store and interpolates as the
    /// store path, and the same part inside a *path* is not copied and
    /// contributes its own spelling. Sniffing the first value at runtime
    /// cannot tell them apart -- `"${./f}"` also has a path first and must
    /// still copy.
    ///
    /// The first value is always a `Const::Path` the compiler emitted, since
    /// the grammar admits nothing else in that position.
    ConcatPath {
        n: u16,
    },
}

/// The variant of an [`Op`] with its operands dropped: what a counter, a
/// coverage check or a histogram groups by.
///
/// A separate fieldless enum rather than a hand-numbered table because the
/// numbering is the part that goes wrong. `ConcatPath` was once given the
/// tag `DerivationGlobal` already held, and the round-trip test could not
/// see it: the wrong decode re-encoded to the same bytes. Here the
/// discriminants are the compiler's, in declaration order, so two kinds
/// cannot collide however the list is edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OpKind {
    Const,
    GetLocal,
    GetLocalLazy,
    Builtin,
    BuiltinsSet,
    DerivationGlobal,
    NixPathGlobal,
    Thunk,
    Closure,
    Apply,
    PushEnv,
    PopEnv,
    JumpIfFalse,
    Jump,
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    Neq,
    Lt,
    Leq,
    Gt,
    Geq,
    Not,
    Negate,
    ConcatStrings,
    MkList,
    ConcatLists,
    MkAttrs,
    Update,
    Select,
    SelectSoft,
    SelectSoftDyn,
    OrDefault,
    HasAttr,
    SelectDyn,
    HasAttrDyn,
    PushWith,
    ResolveWith,
    CallBuiltin,
    Assert,
    Ret,
    ConcatPath,
    MkAttrsOnto,
}

impl OpKind {
    /// Every kind, in discriminant order, so `ALL[i] as usize == i`.
    ///
    /// `kinds_are_their_own_indices` holds that invariant, which is what
    /// lets a counter array be indexed by `kind as usize` without a lookup.
    pub const ALL: &'static [OpKind] = &[
        OpKind::Const,
        OpKind::GetLocal,
        OpKind::GetLocalLazy,
        OpKind::Builtin,
        OpKind::BuiltinsSet,
        OpKind::DerivationGlobal,
        OpKind::NixPathGlobal,
        OpKind::Thunk,
        OpKind::Closure,
        OpKind::Apply,
        OpKind::PushEnv,
        OpKind::PopEnv,
        OpKind::JumpIfFalse,
        OpKind::Jump,
        OpKind::Add,
        OpKind::Sub,
        OpKind::Mul,
        OpKind::Div,
        OpKind::Eq,
        OpKind::Neq,
        OpKind::Lt,
        OpKind::Leq,
        OpKind::Gt,
        OpKind::Geq,
        OpKind::Not,
        OpKind::Negate,
        OpKind::ConcatStrings,
        OpKind::MkList,
        OpKind::ConcatLists,
        OpKind::MkAttrs,
        OpKind::Update,
        OpKind::Select,
        OpKind::SelectSoft,
        OpKind::SelectSoftDyn,
        OpKind::OrDefault,
        OpKind::HasAttr,
        OpKind::SelectDyn,
        OpKind::HasAttrDyn,
        OpKind::PushWith,
        OpKind::ResolveWith,
        OpKind::CallBuiltin,
        OpKind::Assert,
        OpKind::Ret,
        OpKind::ConcatPath,
        OpKind::MkAttrsOnto,
    ];

    /// How wide an array indexed by [`OpKind`] has to be.
    pub const COUNT: usize = OpKind::ALL.len();

    /// The variant's name, matching the spelling in [`Op`].
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            OpKind::Const => "Const",
            OpKind::GetLocal => "GetLocal",
            OpKind::GetLocalLazy => "GetLocalLazy",
            OpKind::Builtin => "Builtin",
            OpKind::BuiltinsSet => "BuiltinsSet",
            OpKind::DerivationGlobal => "DerivationGlobal",
            OpKind::NixPathGlobal => "NixPathGlobal",
            OpKind::Thunk => "Thunk",
            OpKind::Closure => "Closure",
            OpKind::Apply => "Apply",
            OpKind::PushEnv => "PushEnv",
            OpKind::PopEnv => "PopEnv",
            OpKind::JumpIfFalse => "JumpIfFalse",
            OpKind::Jump => "Jump",
            OpKind::Add => "Add",
            OpKind::Sub => "Sub",
            OpKind::Mul => "Mul",
            OpKind::Div => "Div",
            OpKind::Eq => "Eq",
            OpKind::Neq => "Neq",
            OpKind::Lt => "Lt",
            OpKind::Leq => "Leq",
            OpKind::Gt => "Gt",
            OpKind::Geq => "Geq",
            OpKind::Not => "Not",
            OpKind::Negate => "Negate",
            OpKind::ConcatStrings => "ConcatStrings",
            OpKind::MkList => "MkList",
            OpKind::ConcatLists => "ConcatLists",
            OpKind::MkAttrs => "MkAttrs",
            OpKind::MkAttrsOnto => "MkAttrsOnto",
            OpKind::Update => "Update",
            OpKind::Select => "Select",
            OpKind::SelectSoft => "SelectSoft",
            OpKind::SelectSoftDyn => "SelectSoftDyn",
            OpKind::OrDefault => "OrDefault",
            OpKind::HasAttr => "HasAttr",
            OpKind::SelectDyn => "SelectDyn",
            OpKind::HasAttrDyn => "HasAttrDyn",
            OpKind::PushWith => "PushWith",
            OpKind::ResolveWith => "ResolveWith",
            OpKind::CallBuiltin => "CallBuiltin",
            OpKind::Assert => "Assert",
            OpKind::Ret => "Ret",
            OpKind::ConcatPath => "ConcatPath",
        }
    }
}

impl Op {
    /// This op's kind. An exhaustive match, so a variant added to [`Op`]
    /// does not compile until someone decides which kind counts it.
    #[must_use]
    pub const fn kind(&self) -> OpKind {
        match self {
            Op::Const(_) => OpKind::Const,
            Op::GetLocal { .. } => OpKind::GetLocal,
            Op::GetLocalLazy { .. } => OpKind::GetLocalLazy,
            Op::Builtin { .. } => OpKind::Builtin,
            Op::BuiltinsSet => OpKind::BuiltinsSet,
            Op::DerivationGlobal => OpKind::DerivationGlobal,
            Op::NixPathGlobal => OpKind::NixPathGlobal,
            Op::Thunk { .. } => OpKind::Thunk,
            Op::Closure { .. } => OpKind::Closure,
            Op::Apply => OpKind::Apply,
            Op::PushEnv { .. } => OpKind::PushEnv,
            Op::PopEnv => OpKind::PopEnv,
            Op::JumpIfFalse { .. } => OpKind::JumpIfFalse,
            Op::Jump { .. } => OpKind::Jump,
            Op::Add => OpKind::Add,
            Op::Sub => OpKind::Sub,
            Op::Mul => OpKind::Mul,
            Op::Div => OpKind::Div,
            Op::Eq => OpKind::Eq,
            Op::Neq => OpKind::Neq,
            Op::Lt => OpKind::Lt,
            Op::Leq => OpKind::Leq,
            Op::Gt => OpKind::Gt,
            Op::Geq => OpKind::Geq,
            Op::Not => OpKind::Not,
            Op::Negate => OpKind::Negate,
            Op::ConcatStrings { .. } => OpKind::ConcatStrings,
            Op::MkList { .. } => OpKind::MkList,
            Op::ConcatLists => OpKind::ConcatLists,
            Op::MkAttrs { .. } => OpKind::MkAttrs,
            Op::MkAttrsOnto { .. } => OpKind::MkAttrsOnto,
            Op::Update => OpKind::Update,
            Op::Select { .. } => OpKind::Select,
            Op::SelectSoft { .. } => OpKind::SelectSoft,
            Op::SelectSoftDyn => OpKind::SelectSoftDyn,
            Op::OrDefault => OpKind::OrDefault,
            Op::HasAttr { .. } => OpKind::HasAttr,
            Op::SelectDyn => OpKind::SelectDyn,
            Op::HasAttrDyn => OpKind::HasAttrDyn,
            Op::PushWith => OpKind::PushWith,
            Op::ResolveWith { .. } => OpKind::ResolveWith,
            Op::CallBuiltin { .. } => OpKind::CallBuiltin,
            Op::Assert => OpKind::Assert,
            Op::Ret => OpKind::Ret,
            Op::ConcatPath { .. } => OpKind::ConcatPath,
        }
    }
}

/// Pure literals only.
///
/// # Equality is bit-exact, floats included
///
/// [`Eq`] and [`Hash`] are what index the compiler's constant pool
/// (`Compiler::konst`), and a derived float `PartialEq` is not an
/// equivalence relation: `NaN != NaN` makes it irreflexive, so `Eq` would
/// be a lie and a hash lookup could miss an entry byte-identical to its key.
/// Comparing float bits fixes both, and it is the reading [`crate::modcache`]
/// already serializes (`f64::to_be_bytes`), so the pool's in-memory identity
/// and its on-disk identity agree instead of drifting.
///
/// Nothing observable rests on the choice, and it is not a bug fix. The lexer
/// hands the compiler neither a NaN nor a signed zero -- `-0.0` parses as
/// negation applied to `0.0` and compiles to `Op::Negate`, never to a
/// constant -- so the cases where the two readings differ cannot reach this
/// pool from source. Two pool entries holding equal values would behave
/// identically at run time in any case: deduplication here is a size
/// optimization, not a semantic one.
#[derive(Debug, Clone)]
pub enum Const {
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    /// String without context (context arises only at runtime).
    Str(String),
    /// Path literal, already made absolute by the compiler against the
    /// module's base directory, with the accessor root selected at parse
    /// time. Relative literals use [`Module::root`]; absolute and home
    /// literals use the ambient root, as cppnix's parser does.
    Path(crate::value2::PathValue),
}

impl PartialEq for Const {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a.to_bits() == b.to_bits(),
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Null, Self::Null) => true,
            (Self::Str(a), Self::Str(b)) => a == b,
            (Self::Path(a), Self::Path(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for Const {}

impl std::hash::Hash for Const {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // The discriminant first, so `Str("/x")` and `Path("/x")` -- which
        // are different constants and compile to different values -- do not
        // collide on the payload they share.
        std::mem::discriminant(self).hash(state);
        match self {
            Self::Int(i) => i.hash(state),
            Self::Float(f) => f.to_bits().hash(state),
            Self::Bool(b) => b.hash(state),
            Self::Null => {}
            Self::Str(s) => s.hash(state),
            Self::Path(p) => p.hash(state),
        }
    }
}

/// One compiled body: the top-level expression or one lambda.
#[derive(Debug, Clone, Default)]
pub struct CodeUnit {
    pub ops: Vec<Op>,
    /// Formal parameter shape for lambda units.
    pub param: Option<Param>,
    /// Where each op in `ops` came from: `spans[i]` is the byte offset in the
    /// module's source of the construct that emitted `ops[i]`, or [`NO_POS`].
    ///
    /// A side table and not a field on [`Op`], deliberately. `Op` is a
    /// fixed-width `Copy` enum the interpreter fetches in its innermost loop;
    /// widening every instruction by four bytes to serve a path that only
    /// runs when an evaluation is already failing would put the cost on every
    /// evaluation that does not. Nothing in the interpreter reads this array
    /// except [`crate::vm::Vm::unwind`] and `builtins.unsafeGetAttrPos`, so
    /// its cache lines are never touched on a successful run.
    ///
    /// Parallel to `ops` by construction (`Emit::push` appends to both), and
    /// `spans_cover_every_op` in `compile` holds that for compiled modules.
    /// A decoder that produced a short one would be a bug, so the readers
    /// treat a missing entry as [`NO_POS`] rather than panicking.
    pub spans: Vec<u32>,
    /// Where the attributes each `MkAttrs` in `ops` builds were written, for
    /// `builtins.unsafeGetAttrPos`. Only the ops that build a set have an
    /// entry, so a unit with no set literal in it carries an empty vector.
    /// Sorted by `ip`.
    pub attr_sites: Vec<AttrSite>,
}

impl CodeUnit {
    /// The compiler<->VM contract on `attr_sites`, checked structurally.
    ///
    /// The VM takes a set's static NAMES from its site, so a site whose static
    /// half does not match the op it describes names every value wrong,
    /// silently. The compiler test runs this on every fixture and the module
    /// decoder runs it on every cached unit, so a stale or damaged cache row
    /// fails closed instead of evaluating to a wrong set.
    ///
    /// Checked: sites strictly increasing by `ip` (the VM binary-searches
    /// them); each site sits on `MkAttrs`/`MkAttrsOnto`, names as many static
    /// values as that op pops (the op carries no names, so the table IS their
    /// identity) with every symbol inside the module's table, and indexes
    /// exactly its dynamic pairs, in order; each site's `by_name` is its static half permuted into text
    /// order (`AttrSite::static_offset` binary-searches it); and every
    /// set-building op that builds anything has a site (`Emit::attr_site`
    /// files none for `{}`).
    pub fn check_attr_sites(&self, symbols: &[String]) -> Result<(), String> {
        let mut prev_ip: Option<u32> = None;
        for site in &self.attr_sites {
            if prev_ip.is_some_and(|p| p >= site.ip) {
                return Err(format!("attr sites out of ip order at ip {}", site.ip));
            }
            prev_ip = Some(site.ip);
            let (statics, dynamics) = match self.ops.get(site.ip as usize) {
                Some(Op::MkAttrs { statics, dynamics }) => (*statics, *dynamics),
                Some(Op::MkAttrsOnto { n }) => (0, *n),
                other => {
                    return Err(format!(
                        "attr site at ip {} describes {other:?}, not a set-building op",
                        site.ip
                    ));
                }
            };
            if site.static_names().len() != usize::from(statics) {
                return Err(format!(
                    "attr site at ip {} names {} static attributes, the op pops {statics}",
                    site.ip,
                    site.static_names().len()
                ));
            }
            let indexes_pairs = site.dynamic_names().len() == usize::from(dynamics)
                && site
                    .dynamic_names()
                    .iter()
                    .enumerate()
                    .all(|(i, &(pair, _))| usize::try_from(pair) == Ok(i));
            if !indexes_pairs {
                return Err(format!(
                    "attr site at ip {} does not index the op's {dynamics} dynamic pairs in order",
                    site.ip
                ));
            }
            if let Some(&(sym, _)) = site
                .static_names()
                .iter()
                .find(|&&(sym, _)| usize::try_from(sym).is_ok_and(|s| s >= symbols.len()))
            {
                return Err(format!(
                    "attr site at ip {} names symbol {sym}, the module has {}",
                    site.ip,
                    symbols.len()
                ));
            }
            if !site.indexed_by_text(symbols) {
                return Err(format!(
                    "attr site at ip {} is not indexed by text: {:?} over {statics} static names",
                    site.ip, site.by_name
                ));
            }
        }
        for (ip, op) in self.ops.iter().enumerate() {
            let builds = match op {
                Op::MkAttrs { statics, dynamics } => *statics > 0 || *dynamics > 0,
                Op::MkAttrsOnto { n } => *n > 0,
                _ => continue,
            };
            let has_site = u32::try_from(ip)
                .is_ok_and(|ip| self.attr_sites.binary_search_by_key(&ip, |s| s.ip).is_ok());
            if builds != has_site {
                return Err(format!(
                    "{op:?} at ip {ip} builds={builds} but has a site={has_site}; a set-building op \
                     has a site iff it builds something"
                ));
            }
        }
        Ok(())
    }

    /// Index every site's static names by text, then check the contract: the
    /// one call the compiler and the module decoder each make once the unit
    /// and the module's symbols are complete.
    pub fn link_attr_sites(&mut self, symbols: &[String]) -> Result<(), String> {
        for site in &mut self.attr_sites {
            site.index(symbols);
        }
        self.check_attr_sites(symbols)
    }
}

/// The attribute names one `MkAttrs` builds, and where each was written.
///
/// The first `static_count` entries in `names` are static `(module symbol,
/// byte offset)` pairs in the order the op pops its static values (emission
/// order: every `inherit` first, then the bindings, each in source order):
/// the VM zips the two, so this is the op's name table and not only its
/// position table. The rest are runtime-computed `(index among the op's
/// dynamic pairs, byte offset)` entries, one per dynamic pair in order. One
/// vector, a split index and the boxed text index keep `AttrSite` at 48
/// bytes while letting `MkAttrs` record only the evaluated dynamic names on
/// each run.
/// [`CodeUnit::check_attr_sites`] states the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrSite {
    /// Index into the unit's `ops` of the `MkAttrs` this describes.
    pub ip: u32,
    /// Static entries followed by dynamic entries. See the type-level comment.
    pub names: Vec<(u32, u32)>,
    /// Where the dynamic suffix of `names` begins.
    pub static_count: u32,
    /// Indices into the static half of `names`, ordered by the symbol's text,
    /// so a position lookup (which arrives with text) is a binary search
    /// over a table whose own order is the op's. Derived, never encoded:
    /// [`CodeUnit::link_attr_sites`] builds it from the module's symbols
    /// after compile and after decode.
    pub by_name: Box<[u16]>,
}

impl AttrSite {
    fn static_len(&self) -> usize {
        usize::try_from(self.static_count)
            .unwrap_or(self.names.len())
            .min(self.names.len())
    }

    fn text<'a>(&self, i: u16, symbols: &'a [String]) -> &'a str {
        self.static_names()
            .get(usize::from(i))
            .and_then(|&(sym, _)| symbols.get(sym as usize))
            .map_or("", String::as_str)
    }

    /// Build `by_name` from `symbols`. `Op::MkAttrs` counts its statics in a
    /// `u16`, so the indices fit; a wider static half leaves the index empty
    /// and [`CodeUnit::check_attr_sites`] refuses the unit.
    fn index(&mut self, symbols: &[String]) {
        let mut by_name: Vec<u16> = (0..self.static_len())
            .map(u16::try_from)
            .collect::<Result<_, _>>()
            .unwrap_or_default();
        by_name.sort_by(|&a, &b| self.text(a, symbols).cmp(self.text(b, symbols)));
        self.by_name = by_name.into_boxed_slice();
    }

    /// Whether `by_name` is a permutation of the static half ordered by text.
    fn indexed_by_text(&self, symbols: &[String]) -> bool {
        let n = self.static_len();
        let mut seen = vec![false; n];
        let permutes = self.by_name.len() == n
            && self.by_name.iter().all(|&i| {
                let slot = seen.get_mut(usize::from(i));
                slot.is_some_and(|s| !std::mem::replace(s, true))
            });
        permutes && self.by_name.is_sorted_by_key(|&i| self.text(i, symbols))
    }

    /// The byte offset of static attribute `name`, or `None` when this site
    /// has no static attribute of that name.
    #[must_use]
    pub fn static_offset(&self, name: &str, symbols: &[String]) -> Option<u32> {
        let i = self
            .by_name
            .binary_search_by(|&i| self.text(i, symbols).cmp(name))
            .ok()?;
        self.static_names()
            .get(usize::from(*self.by_name.get(i)?))
            .map(|&(_, offset)| offset)
    }

    #[must_use]
    pub fn static_names(&self) -> &[(u32, u32)] {
        self.names.split_at(self.static_len()).0
    }

    #[must_use]
    pub fn dynamic_names(&self) -> &[(u32, u32)] {
        self.names.split_at(self.static_len()).1
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    /// `x: body`; the argument binds one local slot.
    Ident(u32),
    /// `{ a, b ? d, ... } @ bind: body`: the formals in source order, the
    /// ellipsis flag, and the optional @-binding symbol.
    Formals {
        fields: Vec<Formal>,
        ellipsis: bool,
        bind: Option<u32>,
    },
}

/// One `{ a, b ? d }` formal: the parameter name, the unit computing its
/// default, and where the name was written.
///
/// A struct rather than the `(u32, Option<u32>)` pair it grew out of because
/// the third field is a position and reads as neither of the first two. The
/// position is here and not in a side table because `builtins.functionArgs`
/// hands the formals back as an attribute set whose attributes cppnix gives
/// the formals' own positions (`primops.cc`'s `prim_functionArgs`), so
/// `unsafeGetAttrPos` on that set has to reach them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Formal {
    pub sym: u32,
    /// The unit evaluating `? d`, or `None` for a formal with no default.
    pub default: Option<u32>,
    /// Byte offset of the name token in the module's source, or [`NO_POS`].
    pub pos: u32,
}

/// A compiled source file. Symbols are module-local; the VM re-interns at
/// load. Everything is indexed, so serialization is a straight walk.
#[derive(Debug, Clone, Default)]
pub struct Module {
    pub consts: Vec<Const>,
    pub symbols: Vec<String>,
    pub units: Vec<CodeUnit>,
    /// Index of the entry unit (the file's top-level expression).
    pub entry: u32,
    /// The accessor root of the source file this module was compiled from.
    /// Relative path literals copy it into their constant; keeping it on the
    /// module also makes imported-source provenance part of the compiled
    /// module's identity.
    pub root: crate::value2::Root,
    /// What a position into this module names when it is printed.
    pub origin: SrcOrigin,
    /// Byte offset of the first character of every line of the source, so a
    /// byte offset in [`CodeUnit::spans`] resolves to a line and a column
    /// without the source text.
    ///
    /// The text itself is deliberately not kept: it is the largest thing
    /// there is and the VM would only ever use it to count newlines, which is
    /// this array. cppnix builds the same array lazily off the file it
    /// re-reads (`PosTable::operator[]`); here the file cannot be re-read,
    /// because the VM performs no IO.
    pub line_starts: Vec<u32>,
    /// Runtime-only dynamic, `listToAttrs`, and projected `Update` attribute
    /// origins. Module encoding omits this slab, and cloning a compiled module
    /// starts with an empty one.
    pub(crate) dynamic_attr_origins: crate::value2::DynamicAttrOriginSlab,
    /// This module's symbols in the running VM's numbering, linked on first
    /// use. Runtime-only like the slab above; see [`crate::value2::LinkedSymbols`].
    pub(crate) linked: crate::value2::LinkedSymbols,
}

/// Where the text a module was compiled from came from.
///
/// The same distinction cppnix draws on its `PosTable`, and it is observable
/// twice over: an error prints `«string»` for text with no file behind it
/// (`Pos::print`), and `builtins.unsafeGetAttrPos` answers `null` rather than
/// a record for an attribute in such text, because `mkPos` only builds one
/// for a `SourcePath` origin (`eval.cc`'s `mkPos`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SrcOrigin {
    /// `--expr`, a REPL line, one of this crate's embedded wrappers.
    #[default]
    String,
    /// A file, named by the absolute path cppnix would print. Not
    /// canonicalised: cppnix reports the path it resolved.
    File(String),
}

/// A [`CodeUnit::spans`] or [`Formal::pos`] entry for a construct with no
/// position: a compiler-synthesised op, or a decoder filling a gap.
///
/// `u32::MAX` rather than `0`, because `0` is a real byte offset -- the first
/// character of the file -- and a sentinel that collides with a legitimate
/// value is a sentinel that silently reports the top of the file for
/// everything the compiler forgot.
pub const NO_POS: u32 = u32::MAX;

impl Module {
    /// Whether module symbol `sym` spells `name`. The formals table is in
    /// source order, not sorted, and a position lookup arrives with text, so
    /// its scan compares through this. (Attr sites carry a text index:
    /// [`AttrSite::static_offset`].)
    #[must_use]
    pub(crate) fn symbol_is(&self, sym: u32, name: &str) -> bool {
        self.symbols.get(sym as usize).is_some_and(|s| s == name)
    }

    /// The 1-based line and column cppnix would print for a byte offset, or
    /// `None` when there is no position.
    ///
    /// The column is counted in bytes, as cppnix's is: `PosTable::operator[]`
    /// is `1 + (offset - lineStart)` over a byte offset, so a line with
    /// multi-byte characters before the column reports the same number on
    /// both evaluators only if this one also counts bytes.
    #[must_use]
    pub fn line_col(&self, offset: u32) -> Option<(u32, u32)> {
        if offset == NO_POS {
            return None;
        }
        // `partition_point` is the `upper_bound` cppnix takes and then steps
        // back from: the number of line starts at or before `offset`.
        let line = self.line_starts.partition_point(|&start| start <= offset);
        let start = *self.line_starts.get(line.checked_sub(1)?)?;
        Some((u32::try_from(line).ok()?, offset.saturating_sub(start) + 1))
    }

    /// The line starts of `src`, in the shape [`Module::line_col`] wants.
    ///
    /// A line ends at `\n`, at `\r\n`, or at a bare `\r`, which is what
    /// `Pos::LinesIterator` accepts and what its own comment says the parser
    /// assumes. Counting only `\n` would report a line number cppnix does
    /// not for any file written on a classic Mac or by a tool that emits
    /// `\r`, and the two evaluators' error positions would silently drift
    /// apart on exactly those files.
    #[must_use]
    pub fn line_starts_of(src: &str) -> Vec<u32> {
        let bytes = src.as_bytes();
        let mut starts = vec![0u32];
        let mut i = 0;
        while i < bytes.len() {
            match bytes.get(i) {
                Some(b'\n') => i += 1,
                Some(b'\r') => {
                    i += if bytes.get(i + 1) == Some(&b'\n') {
                        2
                    } else {
                        1
                    }
                }
                _ => {
                    i += 1;
                    continue;
                }
            }
            // A break at the very end of the file opens no further line, and
            // cppnix's iterator does not report one either.
            if i < bytes.len() {
                starts.push(u32::try_from(i).unwrap_or(NO_POS));
            }
        }
        starts
    }
}

/// One value per [`Op`] variant.
///
/// A match cannot be iterated, so the only way to reach every arm of
/// [`Op::kind`] is to hand it one of each. The return type is
/// `[Op; OpKind::COUNT]`, which is what ties this to the kind list: adding a
/// kind without adding a sample fails to compile rather than silently
/// covering one op fewer.
#[cfg(test)]
pub(crate) fn one_of_each() -> [Op; OpKind::COUNT] {
    [
        Op::Const(0),
        Op::GetLocal { depth: 0, slot: 0 },
        Op::GetLocalLazy { depth: 0, slot: 0 },
        Op::Builtin { idx: 0 },
        Op::BuiltinsSet,
        Op::DerivationGlobal,
        Op::NixPathGlobal,
        Op::Thunk { unit: 0 },
        Op::Closure { unit: 0 },
        Op::Apply,
        Op::PushEnv { n: 0 },
        Op::PopEnv,
        Op::JumpIfFalse { target: 0 },
        Op::Jump { target: 0 },
        Op::Add,
        Op::Sub,
        Op::Mul,
        Op::Div,
        Op::Eq,
        Op::Neq,
        Op::Lt,
        Op::Leq,
        Op::Gt,
        Op::Geq,
        Op::Not,
        Op::Negate,
        Op::ConcatStrings { n: 0 },
        Op::MkList { n: 0 },
        Op::ConcatLists,
        Op::MkAttrs {
            statics: 0,
            dynamics: 0,
        },
        Op::Update,
        Op::Select { sym: 0 },
        Op::SelectSoft { sym: 0 },
        Op::SelectSoftDyn,
        Op::OrDefault,
        Op::HasAttr { sym: 0 },
        Op::SelectDyn,
        Op::HasAttrDyn,
        Op::PushWith,
        Op::ResolveWith { sym: 0 },
        Op::CallBuiltin { idx: 0 },
        Op::Assert,
        Op::Ret,
        Op::ConcatPath { n: 0 },
        Op::MkAttrsOnto { n: 0 },
    ]
}

#[cfg(test)]
mod line_table_tests {
    use super::{Module, NO_POS};

    fn module(src: &str) -> Module {
        Module {
            line_starts: Module::line_starts_of(src),
            ..Module::default()
        }
    }

    /// Every byte offset of `src` resolves to the line and column that
    /// counting by hand gives, for all three line terminators at once.
    #[test]
    fn every_offset_resolves_the_way_counting_by_hand_does() {
        for src in [
            "a\nbb\nccc",
            "a\r\nbb\r\nccc",
            "a\rbb\rccc",
            "",
            "\n",
            "abc",
        ] {
            let m = module(src);
            let (mut line, mut column) = (1u32, 1u32);
            let bytes = src.as_bytes();
            for offset in 0..bytes.len() {
                assert_eq!(
                    m.line_col(u32::try_from(offset).unwrap_or(NO_POS)),
                    Some((line, column)),
                    "offset {offset} of {src:?}"
                );
                match (bytes.get(offset), bytes.get(offset + 1)) {
                    // The `\n` of a `\r\n` is part of the break that already
                    // happened, so it does not open a second line.
                    (Some(b'\r'), Some(b'\n')) => column += 1,
                    (Some(b'\n' | b'\r'), _) => {
                        line += 1;
                        column = 1;
                    }
                    _ => column += 1,
                }
            }
        }
    }

    /// A trailing break opens no line. cppnix's `Pos::LinesIterator` reports
    /// two lines for `"a\nb\n"`, not three, so a `line_starts` with a third
    /// entry would report a line past the end of the file for nothing.
    #[test]
    fn a_trailing_break_opens_no_line() {
        assert_eq!(module("a\nb\n").line_starts.len(), 2);
        assert_eq!(module("a\nb").line_starts.len(), 2);
        assert_eq!(module("").line_starts.len(), 1);
    }

    /// Columns are byte counts, as cppnix's are, so a multi-byte character
    /// before the column moves it by its length in bytes.
    #[test]
    fn columns_are_byte_counts() {
        let src = "\u{e9}x";
        assert_eq!(module(src).line_col(2), Some((1, 3)));
    }

    /// [`NO_POS`] is not a byte offset, and must not read as the top of the
    /// file. A sentinel of `0` would, which is why it is `u32::MAX`.
    #[test]
    fn the_sentinel_is_not_a_position() {
        assert_eq!(module("abc").line_col(NO_POS), None);
        assert_eq!(module("abc").line_col(0), Some((1, 1)));
    }
}

#[cfg(test)]
mod tests {
    use super::{AttrSite, CodeUnit, NO_POS, Op, OpKind, one_of_each};
    use std::collections::BTreeSet;

    /// `OpKind::ALL` is written out by hand, so it can drift from the enum in
    /// the two ways that matter: an entry out of order, and an entry missing.
    /// Both break `kind as usize` as an array index, which is the only reason
    /// the list exists.
    #[test]
    fn kinds_are_their_own_indices() {
        for (i, kind) in OpKind::ALL.iter().enumerate() {
            assert_eq!(
                *kind as usize, i,
                "OpKind::ALL[{i}] is {kind:?}, whose discriminant is {}; the \
                 list is out of declaration order, so a counter array indexed \
                 by `kind as usize` files counts under the wrong name",
                *kind as usize
            );
        }
    }

    /// Every kind is reachable from some op, and every op's kind is listed.
    ///
    /// This is what catches a variant appended to `OpKind` and forgotten in
    /// `ALL`: the sample's kind is then a discriminant past the end of the
    /// list, and `kinds_are_their_own_indices` cannot see it because it only
    /// walks the entries that are there.
    #[test]
    fn every_op_kind_is_listed_and_sampled() {
        let listed: BTreeSet<OpKind> = OpKind::ALL.iter().copied().collect();
        assert_eq!(
            listed.len(),
            OpKind::ALL.len(),
            "OpKind::ALL repeats a kind"
        );

        let sampled: BTreeSet<OpKind> = one_of_each().iter().map(Op::kind).collect();
        assert_eq!(
            sampled, listed,
            "the ops sampled and the kinds listed disagree; a kind added to \
             the enum needs an entry in ALL and a sample in one_of_each"
        );
    }

    /// Names are what a counter line is read by, so two kinds sharing one is
    /// two numbers landing in one field.
    #[test]
    fn kind_names_are_distinct_and_match_the_variant() {
        let names: BTreeSet<&str> = OpKind::ALL.iter().map(|k| k.name()).collect();
        assert_eq!(names.len(), OpKind::ALL.len(), "two kinds share a name");
        assert_eq!(OpKind::Const.name(), "Const");
        assert_eq!(OpKind::ConcatPath.name(), "ConcatPath");
        assert_eq!(
            Op::MkAttrs {
                statics: 3,
                dynamics: 1,
            }
            .kind()
            .name(),
            "MkAttrs"
        );
    }

    /// One `Vec` plus a split index plus one boxed slice: 48 bytes, and a
    /// unit with no set literal carries none of them.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn attr_site_remains_48_bytes() {
        assert_eq!(std::mem::size_of::<AttrSite>(), 48);
    }

    /// `link_attr_sites` derives the text index, and the checker refuses an
    /// index that is not the text-ordered permutation of the static half:
    /// a scrambled or duplicated index would make `static_offset` answer
    /// with the wrong attribute's position.
    #[test]
    fn the_text_index_is_derived_and_checked() {
        let symbols = vec!["b".to_owned(), "a".to_owned(), "c".to_owned()];
        let mut unit = CodeUnit {
            ops: vec![
                Op::MkAttrs {
                    statics: 3,
                    dynamics: 0,
                },
                Op::Ret,
            ],
            param: None,
            spans: vec![NO_POS; 2],
            attr_sites: vec![AttrSite {
                ip: 0,
                names: vec![(0, 10), (1, 20), (2, 30)],
                static_count: 3,
                by_name: Box::default(),
            }],
        };
        unit.link_attr_sites(&symbols)
            .expect("a well-formed site links");
        let site = &unit.attr_sites[0];
        assert_eq!(&*site.by_name, &[1, 0, 2], "a, b, c");
        assert_eq!(site.static_offset("a", &symbols), Some(20));
        assert_eq!(site.static_offset("b", &symbols), Some(10));
        assert_eq!(site.static_offset("c", &symbols), Some(30));
        assert_eq!(site.static_offset("d", &symbols), None);
        assert_eq!(site.static_offset("", &symbols), None);
        for bad in [vec![0u16, 1, 2], vec![1, 1, 2], vec![1, 0], vec![1, 0, 3]] {
            unit.attr_sites[0].by_name = bad.clone().into_boxed_slice();
            assert!(
                unit.check_attr_sites(&symbols).is_err(),
                "index {bad:?} passed the check"
            );
        }
        // A symbol the module does not have is refused, not read as "".
        unit.attr_sites[0].names[2].0 = 3;
        assert!(
            unit.link_attr_sites(&symbols).is_err(),
            "a static name with symbol 3 of 3 linked"
        );
    }
}
